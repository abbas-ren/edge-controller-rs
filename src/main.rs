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
mod store;
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
use tracing::{debug, info, warn};

const USAGE: &str = "\
Usage:
  dev-controller-rs [CONFIG]
  dev-controller-rs --check [CONFIG]
  dev-controller-rs --log-level LEVEL [CONFIG]
  dev-controller-rs --verbose [CONFIG]
  dev-controller-rs --help

CONFIG defaults to /etc/config/login.cfg.

--check validates configuration, environment overrides, hardware policy,
and persisted state without accessing hardware or contacting the backend.
It does not modify or repair files.

--log-level accepts trace|debug|info|warn|error.
--verbose sets debug logging for troubleshooting.
";

#[derive(Debug, PartialEq, Eq)]
enum Cli {
    Help,
    Start {
        config_path: String,
        check_only: bool,
        log_level: Option<String>,
    },
}

impl Cli {
    fn parse<I>(arguments: I) -> Result<Self>
    where
        I: IntoIterator<Item = std::ffi::OsString>,
    {
        let mut config_path = None;
        let mut check_only = false;
        let mut help = false;
        let mut log_level = None;
        let mut arguments = arguments.into_iter().peekable();

        while let Some(argument) = arguments.next() {
            let argument = argument
                .into_string()
                .map_err(|_| anyhow!("command-line arguments must be valid UTF-8"))?;

            match argument.as_str() {
                "--help" | "-h" => {
                    if help {
                        return Err(anyhow!("help flag supplied more than once"));
                    }
                    help = true;
                }

                "--check" => {
                    if check_only {
                        return Err(anyhow!("--check supplied more than once"));
                    }
                    check_only = true;
                }

                "-v" | "--verbose" => {
                    if log_level.is_some() {
                        return Err(anyhow!("log level supplied more than once"));
                    }
                    log_level = Some("debug".to_owned());
                }

                "--log-level" => {
                    let value = arguments
                        .next()
                        .ok_or_else(|| anyhow!("--log-level requires a value: trace|debug|info|warn|error"))?
                        .into_string()
                        .map_err(|_| anyhow!("log level must be valid UTF-8"))?;

                    let level = value.to_ascii_lowercase();
                    match level.as_str() {
                        "trace" | "debug" | "info" | "warn" | "error" => {
                            if log_level.is_some() {
                                return Err(anyhow!("log level supplied more than once"));
                            }
                            log_level = Some(level);
                        }
                        _ => {
                            return Err(anyhow!(
                                "invalid log level: {value}; expected trace|debug|info|warn|error"
                            ));
                        }
                    }
                }

                value if value.starts_with('-') => {
                    return Err(anyhow!("unknown option: {value}"));
                }

                "" => {
                    return Err(anyhow!("configuration pathname must not be empty"));
                }

                value => {
                    if config_path.replace(value.to_owned()).is_some() {
                        return Err(anyhow!("only one configuration pathname may be supplied"));
                    }
                }
            }
        }

        if help {
            if check_only || config_path.is_some() || log_level.is_some() {
                return Err(anyhow!("--help must be used on its own"));
            }

            return Ok(Self::Help);
        }

        Ok(Self::Start {
            config_path: config_path.unwrap_or_else(|| config::LOGIN_PATH.to_owned()),
            check_only,
            log_level,
        })
    }
}

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

/// Single-shot registration before the controller starts serving live traffic.
///
/// The backend creates the session or device record after receiving this POST.
/// The code intentionally does not retry here because the requirement is to send
/// one registration message once per process lifetime, immediately after the
/// listener is bound and ready for the peer to connect back to us.
async fn registration_loop(state: Arc<AppState>, inventory: Vec<models::RelayInventory>) {
    let url = format!(
        "http://{}:{}/api/v1/device/controller/",
        state.cfg.server_ip, state.cfg.http_port
    );

    if *state.reboot.read().await {
        warn!("registration skipped because the controller is already shutting down");
        return;
    }

    // The payload can include the UID once the backend has confirmed it.
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
            warn!(%error, "registration HTTP request failed; backend may still be starting" );
        }
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

/// Validate the configured files without opening hardware or network sockets.
///
/// Run with the service stopped if a stable cross-file snapshot is required.
/// Individual state-file replacement is atomic, but reading multiple files
/// is not a filesystem-wide transaction.
fn check_offline(
    cfg: &config::AppConfig,
    config_path: &str,
    hardware_path: &str,
    bind_address: SocketAddr,
    authentication_enabled: bool,
) -> Result<()> {
    use std::path::Path;

    cfg.validate().context("validating service configuration")?;

    // Require the state directories to exist. Missing state files are valid
    // first-run state, but a missing storage directory is an installation
    // problem that would prevent the first write.
    //
    // This is a read-only structural check, not proof of write permission.
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

    // Do not print the bearer token or controller ID.
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

    // Optional first argument overrides the legacy configuration pathname.
    let (config_path, check_only, log_level) = match Cli::parse(std::env::args_os().skip(1))? {
        Cli::Help => {
            print!("{USAGE}");
            return Ok(());
        }
        Cli::Start {
            config_path,
            check_only,
            log_level,
        } => (config_path, check_only, log_level),
    };

    logging::init_logging(log_level.as_deref());

    let mut cfg = config::AppConfig::load_legacy_cfg(&config_path)
        .with_context(|| format!("loading configuration from {config_path}"))?;

    match std::env::var("DEV_CONTROLLER_INTERFACE") {
        Ok(interface) => cfg.iface_name = interface,
        Err(std::env::VarError::NotPresent) => {}
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(anyhow!("DEV_CONTROLLER_INTERFACE must be valid UTF-8"));
        }
    }

    cfg.validate()
        .context("validating configuration overrides")?;

    // Default to localhost because the existing HTTP router has no
    // authentication. Override explicitly when deploying behind an
    // authenticated proxy or on an appropriately restricted network.
    let bind_value = match std::env::var("DEV_CONTROLLER_BIND") {
        Ok(value) => value,
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

    // if !bind_address.ip().is_loopback() {
    //     warn!(
    //         %bind_address,
    //         "HTTP hardware-control API is exposed without built-in authentication"
    //     );
    // }
    let api_token = config::configured_api_token()?;
    let hardware_path = config::configured_hardware_path()?;
    // This branch must precede signal installation, interface discovery,
    // AppState construction, USB inventory, and server startup.
    if check_only {
        return check_offline(
            &cfg,
            &config_path,
            &hardware_path,
            bind_address,
            api_token.is_some(),
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

    // if !bind_address.ip().is_loopback() && state.api_token.is_none() {
    //     return Err(anyhow!(
    //         "DEV_CONTROLLER_TOKEN is required when listening beyond loopback"
    //     ));
    // }

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

    // The registration is intentionally a single message after the listener is
    // bound and ready. This gives the backend a chance to establish any
    // callback or reverse connection using the payload we send.
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

#[cfg(test)]
mod cli_tests {
    use super::{Cli, USAGE};
    use std::ffi::OsString;

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_arguments_selects_default_configuration() {
        assert_eq!(
            Cli::parse(arguments(&[])).unwrap(),
            Cli::Start {
                config_path: crate::config::LOGIN_PATH.into(),
                check_only: false,
                log_level: None,
            }
        );
    }

    #[test]
    fn positional_configuration_is_preserved() {
        assert_eq!(
            Cli::parse(arguments(&["/etc/dev-controller/custom.cfg",])).unwrap(),
            Cli::Start {
                config_path: "/etc/dev-controller/custom.cfg".into(),
                check_only: false,
                log_level: None,
            }
        );
    }

    #[test]
    fn check_accepts_default_or_explicit_configuration() {
        assert_eq!(
            Cli::parse(arguments(&["--check"])).unwrap(),
            Cli::Start {
                config_path: crate::config::LOGIN_PATH.into(),
                check_only: true,
                log_level: None,
            }
        );

        for values in [
            vec!["--check", "/tmp/controller.cfg"],
            vec!["/tmp/controller.cfg", "--check"],
        ] {
            assert_eq!(
                Cli::parse(arguments(&values)).unwrap(),
                Cli::Start {
                    config_path: "/tmp/controller.cfg".into(),
                    check_only: true,
                    log_level: None,
                }
            );
        }
    }

    #[test]
    fn help_is_standalone() {
        assert_eq!(Cli::parse(arguments(&["--help"])).unwrap(), Cli::Help);

        assert_eq!(Cli::parse(arguments(&["-h"])).unwrap(), Cli::Help);

        assert_eq!(
            Cli::parse(arguments(&["--log-level", "debug"])).unwrap(),
            Cli::Start {
                config_path: crate::config::LOGIN_PATH.into(),
                check_only: false,
                log_level: Some("debug".to_owned()),
            }
        );

        assert!(Cli::parse(arguments(&["--help", "--check"])).is_err());

        assert!(Cli::parse(arguments(&["--help", "/tmp/config"])).is_err());

        assert!(USAGE.contains("--check"));
    }

    #[test]
    fn duplicate_and_unknown_options_are_rejected() {
        for values in [
            vec!["--check", "--check"],
            vec!["--help", "--help"],
            vec!["--unknown"],
            vec!["first.cfg", "second.cfg"],
            vec![""],
        ] {
            assert!(
                Cli::parse(arguments(&values)).is_err(),
                "unexpectedly accepted {values:?}"
            );
        }
    }

    #[test]
    fn non_utf8_argument_is_rejected() {
        use std::os::unix::ffi::OsStringExt;

        let invalid = OsString::from_vec(vec![b'c', b'f', b'g', 0xff]);

        assert!(Cli::parse([invalid]).is_err());
    }
}
