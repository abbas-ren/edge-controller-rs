use anyhow::{anyhow, Context, Result};
use std::{
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tracing::{info, warn};

use crate::{
    config, hardware, models, observability::metrics::global_metrics, state::AppState, store,
};

/// Read the selected interface's MAC and IPv4 address.
///
/// For a Raspberry Pi, prefer `DEV_CONTROLLER_INTERFACE=eth0` or `wlan0` over
/// the legacy default. This keeps the service portable across host layouts.
pub fn interface_identity(interface: &str) -> Result<(String, String)> {
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

const REGISTRATION_RETRY_INITIAL: Duration = Duration::from_secs(2);
const REGISTRATION_RETRY_MAX: Duration = Duration::from_secs(60);

/// Register the controller, retrying transient backend and transport failures.
///
/// Only one registration task may run per process. That task remains active
/// until the backend accepts the controller or service shutdown begins.
pub async fn registration_loop(state: Arc<AppState>, inventory: Vec<models::RelayInventory>) {
    registration_loop_with_delay(state, inventory, REGISTRATION_RETRY_INITIAL).await;
}

async fn registration_loop_with_delay(
    state: Arc<AppState>,
    inventory: Vec<models::RelayInventory>,
    initial_delay: Duration,
) {
    let url = format!(
        "http://{}:{}/api/v1/device/controller/",
        state.cfg.server_ip, state.cfg.http_port
    );

    if state.jobs.is_closing() {
        warn!("registration skipped because the controller is already shutting down");
        return;
    }

    if !state.try_begin_registration() {
        info!(%url, "backend registration already started for this process; skipping duplicate attempt");
        return;
    }

    let mut retry_delay = initial_delay;

    loop {
        if state.jobs.is_closing() {
            info!("backend registration stopped during service shutdown");
            return;
        }

        let payload = state.registration_payload(inventory.clone()).await;
        info!(%url, "registering controller with backend");

        match state.client.post(&url).json(&payload).send().await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    global_metrics().record_registration_attempt("accepted");
                    info!(status = %status, "controller registration request accepted by backend");
                    return;
                }

                global_metrics().record_registration_attempt("rejected");
                warn!(status = %status, "backend rejected registration request; retrying");
            }
            Err(error) => {
                global_metrics().record_registration_attempt("transport_error");
                warn!(%error, "registration HTTP request failed; retrying");
            }
        }

        tokio::time::sleep(retry_delay).await;
        retry_delay = (retry_delay * 2).min(REGISTRATION_RETRY_MAX);
    }
}

/// Signal all capture workers before awaiting any individual worker.
///
/// Serial reads use a short timeout, allowing workers to observe the stop flag
/// without terminating threads or closing descriptors from elsewhere.
pub async fn stop_captures(state: &AppState) {
    let sessions = {
        let mut active = state.rtos_sessions.lock().await;

        for session in active.values() {
            session.stop.store(true, Ordering::Release);
        }

        let sessions = std::mem::take(&mut *active);
        global_metrics().set_active_captures(0);
        sessions
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

        drop(session);
    }
}

/// Validate the configured files without opening hardware or network sockets.
///
/// Run with the service stopped if a stable cross-file snapshot is required.
/// Individual state-file replacement is atomic, but reading multiple files is
/// not a filesystem-wide transaction.
pub fn check_offline(
    cfg: &config::AppConfig,
    config_path: &str,
    hardware_path: &str,
    bind_address: SocketAddr,
    authentication_enabled: bool,
) -> Result<()> {
    use std::path::Path;

    cfg.validate().context("validating service configuration")?;

    for filename in [
        config::UID_FILE,
        config::USB_MAPPING_FILE,
        config::GEN5_MAPPING_FILE,
    ] {
        let parent = Path::new(filename)
            .parent()
            .ok_or_else(|| anyhow!("state pathname has no parent: {filename}"))?;

        let metadata = std::fs::symlink_metadata(parent)
            .with_context(|| format!("inspecting state directory {}", parent.display()))?;

        if !metadata.file_type().is_dir() {
            return Err(anyhow!(
                "state parent must be a directory, not a symlink or special file: {}",
                parent.display()
            ));
        }
    }

    let policy =
        hardware::HardwarePolicy::load(hardware_path).context("validating hardware policy")?;

    let (usb, gen5) = store::load_mappings(
        Path::new(config::USB_MAPPING_FILE),
        Path::new(config::GEN5_MAPPING_FILE),
        &policy,
    )
    .context("validating persisted mappings")?;

    let uid = store::load_uid(Path::new(config::UID_FILE))
        .context("validating persisted controller ID")?;

    let report = serde_json::json!({
        "status": "valid",
        "configuration": config_path,
        "hardware_policy": hardware_path,
        "generation": cfg.gen.as_int(),
        "interface": cfg.iface_name,
        "listen_address": bind_address.to_string(),
        "authentication_enabled": authentication_enabled,
        "approved_boards": policy.boards.len(),
        "usb_mapping_count": usb.len(),
        "gen5_mapping_count": gen5.len(),
        "controller_id_present": uid.is_some(),
        "hardware_checked": false,
        "network_checked": false,
        "write_permissions_checked": false,
        "files_modified": false
    });

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{interface_identity, registration_loop_with_delay};
    use crate::{
        cli::FeatureFlags,
        config::AppConfig,
        hardware::HardwarePolicy,
        jobs::Jobs,
        models::Generation,
        relay::RelayController,
        state::{AppState, ControllerInfo},
    };
    use axum::{http::StatusCode, routing::post, Router};
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tokio::sync::{Mutex, RwLock};

    #[test]
    fn invalid_interface_names_are_rejected() {
        for value in ["", "..", "not/valid", "bad name"] {
            assert!(
                interface_identity(value).is_err(),
                "accepted invalid value: {value}"
            );
        }
    }

    #[tokio::test]
    async fn registration_retries_until_backend_accepts_request() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let handler_attempts = attempts.clone();
        let app = Router::new().route(
            "/api/v1/device/controller/",
            post(move || {
                let attempts = handler_attempts.clone();
                async move {
                    if attempts.fetch_add(1, Ordering::AcqRel) == 0 {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::OK
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let state = Arc::new(AppState {
            cfg: AppConfig {
                server_ip: "127.0.0.1".into(),
                http_port: port,
                ws_port: 1,
                gen: Generation::Gen4,
                bind_port: 8888,
                iface_name: "eth0".into(),
            },
            controller: RwLock::new(ControllerInfo {
                board_mac: "001122334455".into(),
                board_ip: "127.0.0.1".into(),
                uid: None,
            }),
            usb_map: RwLock::new(HashMap::new()),
            gen5_map: RwLock::new(HashMap::new()),
            rtos_sessions: Mutex::new(HashMap::new()),
            relay: RelayController::new(),
            deletion_requested: AtomicBool::new(false),
            client: reqwest::Client::new(),
            jobs: Jobs::default(),
            api_token: None,
            hardware: HardwarePolicy { boards: Vec::new() },
            features: FeatureFlags::default(),
            registration_done: AtomicBool::new(false),
        });

        tokio::time::timeout(
            Duration::from_secs(1),
            registration_loop_with_delay(state, Vec::new(), Duration::from_millis(5)),
        )
        .await
        .expect("registration should retry promptly");

        assert_eq!(attempts.load(Ordering::Acquire), 2);
        server.abort();
    }
}
