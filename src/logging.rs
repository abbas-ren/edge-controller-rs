use std::{fs, path::{Path, PathBuf}};

use tracing_subscriber::{fmt, EnvFilter};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogOptions {
    pub level: Option<String>,
    pub file_path: Option<PathBuf>,
    pub network_logging: bool,
    pub stream_logging: bool,
}

fn env_flag(name: &str) -> Option<bool> {
    std::env::var(name)
        .ok()
        .map(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

impl LogOptions {
    pub fn from_cli(log_level: Option<&str>, log_file: Option<&str>, network_logging: bool, stream_logging: bool) -> Self {
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

pub fn init_logging(log_level: Option<&str>, log_file: Option<&str>, network_logging: bool, stream_logging: bool) {
    let options = LogOptions::from_cli(log_level, log_file, network_logging, stream_logging);
    let level = options.level.as_deref().unwrap_or("info");
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new(level));

    if let Some(file_path) = &options.file_path {
        if let Some(parent) = file_path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let file = tracing_appender::rolling::never(
            file_path.parent().unwrap_or_else(|| Path::new(".")),
            file_path.file_name().unwrap_or_default(),
        );
        let (non_blocking, _guard) = tracing_appender::non_blocking(file);
        let _ = fmt()
            .with_env_filter(filter.clone())
            .with_writer(non_blocking)
            .with_ansi(false)
            .compact()
            .try_init();
        if network_logging {
            tracing::info!(path = %file_path.display(), "network logging enabled for file output");
        }
        if stream_logging {
            tracing::info!(path = %file_path.display(), "stream logging enabled for file output");
        }
        return;
    }

    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .compact()
        .try_init();

    if network_logging {
        tracing::info!("network logging enabled");
    }
    if stream_logging {
        tracing::info!("stream logging enabled");
    }
}

#[cfg(test)]
mod tests {
    use std::{env, sync::Mutex};

    use super::LogOptions;

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
        assert_eq!(options.file_path.as_deref(), Some(std::path::Path::new("/tmp/cli.log")));
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
        assert_eq!(options.file_path.as_deref(), Some(std::path::Path::new("/tmp/env.log")));
        assert!(options.network_logging);
        assert!(options.stream_logging);

        unsafe {
            env::remove_var("DEV_CONTROLLER_LOG_LEVEL");
            env::remove_var("DEV_CONTROLLER_LOG_FILE");
            env::remove_var("DEV_CONTROLLER_LOG_NETWORK");
            env::remove_var("DEV_CONTROLLER_LOG_STREAM");
        }
    }
}
