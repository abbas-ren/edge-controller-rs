//! Shared defaults and path constants used across the controller service.
//!
//! Keeping all common runtime defaults in one place makes the CLI, logs,
//! configuration, and metrics components easier to extend without repeating
//! magic values across the codebase.
#[allow(dead_code)]
pub const DEFAULT_CONFIG_PATH: &str = "/etc/config/login.cfg";
#[allow(dead_code)]
pub const DEFAULT_BIND_PORT: u16 = 8888;
#[allow(dead_code)]
pub const DEFAULT_METRICS_PORT: u16 = 8081;
#[allow(dead_code)]
pub const DEFAULT_LOG_LEVEL: &str = "info";
#[allow(dead_code)]
pub const DEFAULT_LOG_FILE: &str = "/var/log/dev-controller.log";
#[allow(dead_code)]
pub const DEFAULT_CONTROLLER_HOST: &str = "0.0.0.0";
#[allow(dead_code)]
pub const DEFAULT_HEALTH_PATH: &str = "/health";
#[allow(dead_code)]
pub const DEFAULT_SWAGGER_PATH: &str = "/swagger.json";

#[allow(dead_code)]
pub const API_PREFIX: &str = "/api/v1";
#[allow(dead_code)]
pub const METRICS_ROUTE: &str = "/metrics";
#[allow(dead_code)]
pub const HEALTH_ROUTE: &str = "/health";
#[allow(dead_code)]
pub const STATUS_ROUTE: &str = "/status";
#[allow(dead_code)]
pub const SWAGGER_ROUTE: &str = "/swagger.json";

#[allow(dead_code)]
pub const MAX_CONFIG_BYTES: usize = 16 * 1024;
#[allow(dead_code)]
pub const MAX_LOG_LINE_BYTES: usize = 4 * 1024;
