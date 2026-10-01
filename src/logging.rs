use tracing_subscriber::{fmt, EnvFilter};

pub fn init_logging(log_level: Option<&str>) {
    let env_level = std::env::var("DEV_CONTROLLER_LOG_LEVEL").ok();
    // Prefer the explicit CLI override, then the environment, then the default
    // runtime level. This keeps local debugging easy without forcing a rebuild.
    let level = log_level.or(env_level.as_deref()).unwrap_or("info");

    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new(level));

    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .compact()
        .try_init();
}
