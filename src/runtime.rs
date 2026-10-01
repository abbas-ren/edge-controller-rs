use anyhow::{anyhow, Context, Result};
use std::{
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
};
use tracing::{debug, info, warn};

use crate::{
    config,
    hardware,
    models,
    state::AppState,
    store,
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

/// Single-shot registration before the controller starts serving live traffic.
///
/// The backend creates the session or device record after receiving this POST.
/// The code intentionally does not retry here because the requirement is to send
/// one registration message once per process lifetime, immediately after the
/// listener is bound and ready for the peer to connect back to us.
pub async fn registration_loop(state: Arc<AppState>, inventory: Vec<models::RelayInventory>) {
    let url = format!(
        "http://{}:{}/api/v1/device/controller/",
        state.cfg.server_ip, state.cfg.http_port
    );

    if *state.reboot.read().await {
        warn!("registration skipped because the controller is already shutting down");
        return;
    }

    if !state.try_begin_registration() {
        info!(%url, "backend registration already started for this process; skipping duplicate attempt");
        return;
    }

    let payload = state.registration_payload(inventory).await;

    info!(%url, "registering controller with backend once at process startup");
    debug!(payload = ?payload, "registration payload prepared for backend");

    match serde_json::to_string_pretty(&payload) {
        Ok(json) => info!(body = %json, "backend registration payload"),
        Err(error) => warn!(%error, "failed to pretty-print registration payload for logging"),
    }

    match state.client.post(&url).json(&payload).send().await {
        Ok(response) => {
            let status = response.status();
            match response.text().await {
                Ok(body) => {
                    info!(status = %status, body = %body, "registration response received");

                    if status.is_success() {
                        info!("controller registration request accepted by backend");
                    } else {
                        warn!(status = %status, body = %body, "backend rejected registration request");
                    }
                }
                Err(error) => {
                    warn!(%error, "failed to read backend registration response body");
                }
            }
        }
        Err(error) => {
            warn!(%error, "registration HTTP request failed; backend may still be starting");
        }
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

    let policy = hardware::HardwarePolicy::load(hardware_path)
        .context("validating hardware policy")?;

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
    use super::interface_identity;

    #[test]
    fn invalid_interface_names_are_rejected() {
        for value in ["", "..", "not/valid", "bad name"] {
            assert!(interface_identity(value).is_err(), "accepted invalid value: {value}");
        }
    }
}
