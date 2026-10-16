//! Shared defaults and path constants used across the controller service.
//!
//! Keeping all common runtime defaults in one place makes the CLI, logs,
//! configuration, and metrics components easier to extend without repeating
//! magic values across the codebase.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/config/login.cfg";
pub const DEFAULT_BIND_PORT: u16 = 8888;
pub const DEFAULT_METRICS_PORT: u16 = 8081;

pub const FIRMWARE_ROOT: &str = "/home/racerdc1/RACER_IPL";
pub const LOG_ROOT: &str = "/var/lib/dev-controller/ipl";

pub const FLASHWRITER: &str = "ICUMX_Flash_writer_SCIF_DUMMY_CERT_EB203000_V4H.mot";

pub const MAX_IMAGE_BYTES: u64 = 128 * 1024 * 1024;
