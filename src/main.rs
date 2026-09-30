mod config;
mod error;
mod gpio;
mod hardware;
mod http;
mod ipl;
mod jobs;
mod logging;
mod metrics;
mod models;
mod relay;
mod state;
mod uart;
mod usb;
mod websocket;

use anyhow::{anyhow, Context, Result};
use state::AppState;
use std::{
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tracing::{info, warn};

/// Read the selected interface's MAC and IPv4 address.
///
/// For a Raspberry Pi, use DEV_CONTROLLER_INTERFACE=eth0 or wlan0
/// rather than inheriting the legacy Gen5 default of eno1.
fn interface_identity(interface: &str) -> Result<(String, String)> {
    if interface.is_empty()
        || !interface
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(anyhow!("invalid interface name"));
    }

    let mac = std::fs::read_to_string(format!("/sys/class/net/{interface}/address"))?
        .trim()
        .to_owned();

    let address = if_addrs::get_if_addrs()?
        .into_iter()
        .find(|entry| entry.name == interface && entry.ip().is_ipv4())
        .ok_or_else(|| anyhow!("no IPv4 address found on interface {interface}"))?
        .ip()
        .to_string();

    Ok((mac, address))
}

/// Register once successfully, retrying failed requests with capped backoff.
///
/// Registration does not assign a UID locally. The backend supplies it
/// through the existing /confirmation endpoint.
///
/// Inventory is collected once to avoid repeatedly accessing relay hardware
/// during a backend outage.
async fn registration_loop(state: Arc<AppState>, inventory: Vec<models::RelayInventory>) {
    let url = format!(
        "http://{}:{}/api/v1/device/controller/",
        state.cfg.server_ip, state.cfg.http_port
    );

    let mut delay = Duration::from_secs(2);

    loop {
        if *state.reboot.read().await {
            return;
        }

        // Rebuild the payload on each attempt so a newly confirmed UID
        // can be included in a subsequent retry.
        let payload = state.registration_payload(inventory.clone()).await;

        let result = match state.client.post(&url).json(&payload).send().await {
            Ok(response) => response.error_for_status(),
            Err(error) => Err(error),
        };

        match result {
            Ok(response) => {
                info!(
                    status = %response.status(),
                    "controller registration request accepted"
                );
                return;
            }
            Err(error) => {
                warn!(
                    %error,
                    retry_seconds = delay.as_secs(),
                    "controller registration failed"
                );
            }
        }

        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(60));
    }
}

/// Signal all capture workers before awaiting any individual worker.
///
/// Serial reads use a short timeout, allowing workers to observe the stop
/// flag without terminating threads or closing descriptors from elsewhere.
///
/// No timeout is imposed on joining workers: abandoning a spawn_blocking
/// handle would not actually stop its underlying thread.
async fn stop_captures(state: &AppState) {
    let sessions = {
        let mut active = state.rtos_sessions.lock().await;

        for session in active.values() {
            session.stop.store(true, Ordering::Release);
        }

        std::mem::take(&mut *active)
    };

    for (tty, mut session) in sessions {
        if let Some(handle) = session.handle.take() {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    warn!(%tty, %error, "capture worker stopped with an error");
                }
                Err(error) => {
                    warn!(%tty, %error, "capture worker could not be joined");
                }
            }
        }

        // Dropping the NamedTempFile removes the capture pathname.
        // Capture files are not retained across service shutdown.
        drop(session);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    logging::init_logging();

    // Register signal handlers before starting background tasks.
    // SIGTERM is the normal systemd service-stop signal.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing SIGTERM handler")?;

    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("installing SIGINT handler")?;

    // Optional first argument overrides the legacy configuration pathname.
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| config::LOGIN_PATH.to_owned());

    let mut cfg = config::AppConfig::load_legacy_cfg(&config_path)
        .with_context(|| format!("loading configuration from {config_path}"))?;

    if let Ok(interface) = std::env::var("DEV_CONTROLLER_INTERFACE") {
        cfg.iface_name = interface;
    }

    // Default to localhost because the existing HTTP router has no
    // authentication. Override explicitly when deploying behind an
    // authenticated proxy or on an appropriately restricted network.
    let bind_address: SocketAddr = std::env::var("DEV_CONTROLLER_BIND")
        .unwrap_or_else(|_| format!("127.0.0.1:{}", cfg.bind_port))
        .parse()
        .context("DEV_CONTROLLER_BIND must be an IP:port socket address")?;

    if !bind_address.ip().is_loopback() {
        warn!(
            %bind_address,
            "HTTP hardware-control API is exposed without built-in authentication"
        );
    }

    // Network discovery is synchronous and runs before request handling.
    let interface = cfg.iface_name.clone();
    let (mac, ip) = tokio::task::spawn_blocking(move || interface_identity(&interface))
        .await
        .context("network discovery worker failed")??;

    info!(
        interface = %cfg.iface_name,
        %mac,
        %ip,
        generation = cfg.gen.as_int(),
        "controller identity loaded"
    );

    let state = AppState::new(cfg, mac, ip)
        .await
        .context("initializing controller state")?;

    if !bind_address.ip().is_loopback() && state.api_token.is_none() {
        return Err(anyhow!(
            "DEV_CONTROLLER_TOKEN is required when listening beyond loopback"
        ));
    }

    // Bind before registration so the backend can reach /confirmation
    // immediately after receiving the registration request.
    let listener = tokio::net::TcpListener::bind(bind_address)
        .await
        .with_context(|| format!("binding HTTP listener to {bind_address}"))?;

    info!(
        address = %listener.local_addr()?,
        "HTTP listener ready"
    );

    let relay = state.relay.clone();

    let inventory = tokio::task::spawn_blocking(move || relay.inventory())
        .await
        .context("relay inventory worker failed")?;

    let registration_task = tokio::spawn(registration_loop(state.clone(), inventory));
    let websocket_task = tokio::spawn(websocket::websocket_loop(state.clone()));

    let shutdown_state = state.clone();

    let shutdown = async move {
        let deletion_requested = async {
            loop {
                if *shutdown_state.reboot.read().await {
                    break;
                }

                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };

        tokio::select! {
            _ = terminate.recv() => {
                info!("SIGTERM received");
            }
            _ = interrupt.recv() => {
                info!("SIGINT received");
            }
            _ = deletion_requested => {
                info!("controller deletion requested service shutdown");
            }
        }
        shutdown_state.jobs.begin_shutdown();

        // This flag means stop the controller service. It does not reboot
        // Linux or power-cycle any connected board.
        *shutdown_state.reboot.write().await = true;

        // Request capture termination promptly, even while HTTP requests
        // are still draining.
        let sessions = shutdown_state.rtos_sessions.lock().await;
        for session in sessions.values() {
            session.stop.store(true, Ordering::Release);
        }
    };

    let server_result = axum::serve(listener, http::router(state.clone()))
        .with_graceful_shutdown(shutdown)
        .await;
    // Covers both requested shutdown and an unexpected HTTP server failure.
    state.jobs.begin_shutdown();
    *state.reboot.write().await = true;

    // Accepted hardware requests and their child IPL jobs must finish before
    // capture cleanup or GPIO release.
    //
    // This intentionally has no generic forced timeout. Safe interruption of
    // the external Gen5 flashing protocol has not been specified.
    info!("waiting for accepted hardware operations to finish");
    state.jobs.wait().await;

    // Also perform cleanup if the HTTP server exits with an error.
    *state.reboot.write().await = true;

    // The earlier WebSocket loop does not inspect the shutdown flag while
    // connected. Abort its async task to drop the connection promptly.
    registration_task.abort();
    websocket_task.abort();

    for (name, handle) in [
        ("registration", registration_task),
        ("websocket", websocket_task),
    ] {
        if let Err(error) = handle.await {
            if !error.is_cancelled() {
                warn!(task = name, %error, "background task failed");
            }
        }
    }

    stop_captures(&state).await;

    match tokio::task::spawn_blocking(gpio::GpioController::stop).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            warn!(%error, "GPIO release failed");
        }
        Err(error) => {
            warn!(%error, "GPIO cleanup worker failed");
        }
    }

    info!("controller service stopped");
    server_result.context("HTTP server failed")
}
