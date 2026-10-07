use anyhow::{anyhow, Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod runtime;

pub use runtime::{current_level, recent, set_level};

static FILE_LOG_GUARD: OnceLock<tracing_appender::non_blocking::WorkerGuard> = OnceLock::new();

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogOptions {
    pub level: Option<String>,
    pub file_path: Option<PathBuf>,
    pub network_logging: bool,
    pub stream_logging: bool,
}

fn env_flag(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

impl LogOptions {
    pub fn from_cli(
        log_level: Option<&str>,
        log_file: Option<&str>,
        network_logging: bool,
        stream_logging: bool,
    ) -> Self {
        let env_level = std::env::var("DEV_CONTROLLER_LOG_LEVEL").ok();
        let env_file = std::env::var("DEV_CONTROLLER_LOG_FILE").ok();
        let env_network = env_flag("DEV_CONTROLLER_LOG_NETWORK").unwrap_or(false);
        let env_stream = env_flag("DEV_CONTROLLER_LOG_STREAM").unwrap_or(false);

        Self {
            level: log_level
                .map(str::to_owned)
                .or(env_level)
                .or_else(|| Some("info".to_string())),
            file_path: log_file
                .map(PathBuf::from)
                .or_else(|| env_file.map(PathBuf::from)),
            network_logging: network_logging || env_network,
            stream_logging: stream_logging || env_stream,
        }
    }
}

fn logging_filter(options: &LogOptions) -> EnvFilter {
    let level = options.level.as_deref().unwrap_or("info");
    let mut filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new(level));

    if options.network_logging {
        for directive in [
            "edgecontroller::http=debug",
            "edgecontroller::runtime=debug",
            "edgecontroller::websocket=debug",
        ] {
            filter = filter.add_directive(
                directive
                    .parse()
                    .expect("static network log directive must be valid"),
            );
        }
    }

    if options.stream_logging {
        for directive in [
            "edgecontroller::uart=debug",
            "edgecontroller::websocket=debug",
        ] {
            filter = filter.add_directive(
                directive
                    .parse()
                    .expect("static stream log directive must be valid"),
            );
        }
    }

    filter
}

pub fn init_logging(
    log_level: Option<&str>,
    log_file: Option<&str>,
    network_logging: bool,
    stream_logging: bool,
) -> Result<()> {
    let options = LogOptions::from_cli(log_level, log_file, network_logging, stream_logging);
    let filter = logging_filter(&options);
    let initial_level = options.level.as_deref().unwrap_or("info");
    let (filter, capture, runtime_control) = runtime::prepare(filter, initial_level);

    if let Some(file_path) = &options.file_path {
        let directory = file_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let filename = file_path
            .file_name()
            .ok_or_else(|| anyhow!("log file path must include a file name"))?;

        fs::create_dir_all(directory)
            .with_context(|| format!("creating log directory {}", directory.display()))?;

        let file = tracing_appender::rolling::never(directory, filename);
        let (non_blocking, guard) = tracing_appender::non_blocking(file);
        tracing_subscriber::registry()
            .with(filter)
            .with(capture)
            .with(
                fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .compact(),
            )
            .try_init()
            .map_err(|error| anyhow!("installing file log subscriber: {error}"))?;
        runtime::install(runtime_control)?;
        FILE_LOG_GUARD
            .set(guard)
            .map_err(|_| anyhow!("file log worker guard was already installed"))?;
        if options.network_logging {
            tracing::info!(path = %file_path.display(), "network logging enabled for file output");
        }
        if options.stream_logging {
            tracing::info!(path = %file_path.display(), "stream logging enabled for file output");
        }
        return Ok(());
    }

    tracing_subscriber::registry()
        .with(filter)
        .with(capture)
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(false)
                .compact(),
        )
        .try_init()
        .map_err(|error| anyhow!("installing console log subscriber: {error}"))?;
    runtime::install(runtime_control)?;

    if options.network_logging {
        tracing::info!("network logging enabled");
    }
    if options.stream_logging {
        tracing::info!("stream logging enabled");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{env, sync::Mutex};

    use super::{init_logging, logging_filter, LogOptions, FILE_LOG_GUARD};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn cli_flags_override_environment_log_settings() {
        let _guard = ENV_LOCK.lock().unwrap();

        unsafe {
            env::set_var("DEV_CONTROLLER_LOG_LEVEL", "warn");
            env::set_var("DEV_CONTROLLER_LOG_FILE", "/tmp/env.log");
            env::set_var("DEV_CONTROLLER_LOG_NETWORK", "false");
            env::set_var("DEV_CONTROLLER_LOG_STREAM", "0");
        }

        let options = LogOptions::from_cli(Some("debug"), Some("/tmp/cli.log"), true, true);

        assert_eq!(options.level.as_deref(), Some("debug"));
        assert_eq!(
            options.file_path.as_deref(),
            Some(std::path::Path::new("/tmp/cli.log"))
        );
        assert!(options.network_logging);
        assert!(options.stream_logging);

        unsafe {
            env::remove_var("DEV_CONTROLLER_LOG_LEVEL");
            env::remove_var("DEV_CONTROLLER_LOG_FILE");
            env::remove_var("DEV_CONTROLLER_LOG_NETWORK");
            env::remove_var("DEV_CONTROLLER_LOG_STREAM");
        }
    }

    #[test]
    fn environment_variables_enable_debug_logging_when_cli_is_absent() {
        let _guard = ENV_LOCK.lock().unwrap();

        unsafe {
            env::set_var("DEV_CONTROLLER_LOG_LEVEL", "trace");
            env::set_var("DEV_CONTROLLER_LOG_FILE", "/tmp/env.log");
            env::set_var("DEV_CONTROLLER_LOG_NETWORK", "true");
            env::set_var("DEV_CONTROLLER_LOG_STREAM", "yes");
        }

        let options = LogOptions::from_cli(None, None, false, false);

        assert_eq!(options.level.as_deref(), Some("trace"));
        assert_eq!(
            options.file_path.as_deref(),
            Some(std::path::Path::new("/tmp/env.log"))
        );
        assert!(options.network_logging);
        assert!(options.stream_logging);

        unsafe {
            env::remove_var("DEV_CONTROLLER_LOG_LEVEL");
            env::remove_var("DEV_CONTROLLER_LOG_FILE");
            env::remove_var("DEV_CONTROLLER_LOG_NETWORK");
            env::remove_var("DEV_CONTROLLER_LOG_STREAM");
        }
    }

    #[test]
    fn file_logging_retains_its_worker_guard() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("edgecontroller.log");

        init_logging(Some("info"), path.to_str(), false, false).unwrap();

        assert!(FILE_LOG_GUARD.get().is_some());
    }

    #[test]
    fn file_logging_rejects_a_path_without_a_file_name() {
        let error = init_logging(Some("info"), Some("/"), false, false).unwrap_err();

        assert!(error
            .to_string()
            .contains("log file path must include a file name"));
    }

    #[test]
    fn detail_flags_enable_controller_owned_debug_targets() {
        let options = LogOptions {
            level: Some("warn".into()),
            network_logging: true,
            stream_logging: true,
            ..LogOptions::default()
        };

        let filter = logging_filter(&options).to_string();

        for directive in [
            "edgecontroller::http=debug",
            "edgecontroller::runtime=debug",
            "edgecontroller::uart=debug",
            "edgecontroller::websocket=debug",
        ] {
            assert!(filter.contains(directive), "filter omitted {directive}");
        }
    }
}
