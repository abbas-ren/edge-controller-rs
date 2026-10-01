//! Edge controller service for board mapping, relay control, firmware flashing,
//! RTOS capture, backend registration, and operational observability.
//!
//! The binary preserves the legacy controller protocols while applying
//! validated persistence, serialized hardware admission, and graceful
//! shutdown around those operations.

#![deny(missing_docs)]

mod cli;
mod config;
mod constants;
mod error;
mod gpio;
mod hardware;
mod http;
mod ipl;
mod jobs;
mod logging;
mod models;
mod observability;
mod relay;
mod runtime;
mod state;
mod store;
mod uart;
mod usb;
mod websocket;

pub use cli::{Cli, FeatureFlags};

use anyhow::{anyhow, Context, Result};
use state::AppState;
use std::{net::SocketAddr, sync::atomic::Ordering, time::Duration};
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // Logging is initialized before the service starts to ensure startup,
    // registration, and shutdown issues are visible with the requested level.

    // Register signal handlers before starting background tasks.
    // SIGTERM is the normal systemd service-stop signal.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing SIGTERM handler")?;

    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("installing SIGINT handler")?;

    let cli = Cli::parse(std::env::args_os().skip(1))?;

    let (
        config_path,
        check_only,
        log_level,
        log_file,
        log_network,
        log_stream,
        metrics_port,
        bind_port,
        features,
    ) = match cli {
        Cli::Help => {
            print!("{}", Cli::help_text()?);
            return Ok(());
        }
        Cli::Start {
            config_path,
            check_only,
            log_level,
            log_file,
            log_network,
            log_stream,
            metrics_port,
            bind_port,
            features,
        } => (
            config_path,
            check_only,
            log_level,
            log_file,
            log_network,
            log_stream,
            metrics_port,
            bind_port,
            features,
        ),
    };

    logging::init_logging(
        log_level.as_deref(),
        log_file.as_deref(),
        log_network,
        log_stream,
    )?;

    info!(
        config_path = %config_path,
        metrics_port,
        bind_port,
        check_only,
        log_level = log_level.as_deref().unwrap_or("info"),
        log_network,
        log_stream,
        "controller startup configuration selected"
    );

    let mut cfg = config::AppConfig::load_legacy_cfg(&config_path)
        .with_context(|| format!("loading configuration from {config_path}"))?;

    match std::env::var("DEV_CONTROLLER_INTERFACE") {
        Ok(interface) => {
            info!(interface = %interface, "DEV_CONTROLLER_INTERFACE override applied");
            cfg.iface_name = interface;
        }
        Err(std::env::VarError::NotPresent) => {}
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(anyhow!("DEV_CONTROLLER_INTERFACE must be valid UTF-8"));
        }
    }

    cfg.bind_port = bind_port;
    info!(
        configured_interface = %cfg.iface_name,
        configured_bind_port = cfg.bind_port,
        "runtime network configuration prepared"
    );
    cfg.validate()
        .context("validating configuration overrides")?;

    // Preserve the legacy all-interface listener. Deployments should configure
    // a bearer token or restrict access at the network boundary.
    let bind_value = match std::env::var("DEV_CONTROLLER_BIND") {
        Ok(value) => {
            info!(bind = %value, "DEV_CONTROLLER_BIND override applied");
            value
        }
        Err(std::env::VarError::NotPresent) => {
            format!("0.0.0.0:{}", cfg.bind_port)
        }
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(anyhow!("DEV_CONTROLLER_BIND must be valid UTF-8"));
        }
    };

    let bind_address: SocketAddr = bind_value
        .parse()
        .context("DEV_CONTROLLER_BIND must be an IP:port socket address")?;

    if bind_address.port() == 0 {
        return Err(anyhow!(
            "DEV_CONTROLLER_BIND must not use an automatically allocated port"
        ));
    }

    let api_token = config::configured_api_token()?;
    if !bind_address.ip().is_loopback() && api_token.is_none() {
        warn!(
            %bind_address,
            "HTTP hardware-control API is exposed without bearer authentication"
        );
    }
    let hardware_path = config::configured_hardware_path()?;
    // This branch must precede signal installation, interface discovery,
    // AppState construction, USB inventory, and server startup.
    if check_only {
        return runtime::check_offline(
            &cfg,
            &config_path,
            &hardware_path,
            bind_address,
            api_token.is_some(),
        );
    }

    // Network discovery is synchronous and runs before request handling.
    let interface = cfg.iface_name.clone();
    let (mac, ip) = tokio::task::spawn_blocking(move || runtime::interface_identity(&interface))
        .await
        .context("network discovery worker failed")??;

    info!(
        interface = %cfg.iface_name,
        %mac,
        %ip,
        generation = cfg.gen.as_int(),
        "controller identity loaded"
    );

    let state = AppState::new(cfg, mac, ip, features)
        .await
        .context("initializing controller state")?;

    // Bind before registration so the backend can reach /confirmation
    // immediately after receiving the registration request.
    let listener = tokio::net::TcpListener::bind(bind_address)
        .await
        .with_context(|| format!("binding HTTP listener to {bind_address}"))?;

    let metrics_addr = std::net::SocketAddr::from(([0, 0, 0, 0], metrics_port));
    let metrics_task = tokio::spawn(observability::metrics::serve_metrics(metrics_addr));

    info!(
        address = %listener.local_addr()?,
        metrics_address = %metrics_addr,
        "HTTP listener ready"
    );

    let relay = state.relay.clone();

    let inventory = tokio::task::spawn_blocking(move || relay.inventory())
        .await
        .context("relay inventory worker failed")?;

    // Registration begins only after the callback listener is ready and keeps
    // retrying transient backend failures until accepted or shutdown.
    let registration_task = tokio::spawn(runtime::registration_loop(state.clone(), inventory));
    let websocket_task = tokio::spawn(websocket::websocket_loop(state.clone()));

    let shutdown_state = state.clone();

    let shutdown = async move {
        let deletion_requested = async {
            loop {
                if shutdown_state.deletion_requested() {
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

    // Accepted hardware requests and their child IPL jobs must finish before
    // capture cleanup or GPIO release.
    //
    // This intentionally has no generic forced timeout. Safe interruption of
    // the external Gen5 flashing protocol has not been specified.
    info!("waiting for accepted hardware operations to finish");
    state.jobs.wait().await;

    // The earlier WebSocket loop does not inspect the shutdown flag while
    // connected. Abort its async task to drop the connection promptly.
    registration_task.abort();
    websocket_task.abort();

    metrics_task.abort();

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

    runtime::stop_captures(&state).await;

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
