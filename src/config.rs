use crate::error::{AppError, AppResult};
use crate::models::Generation;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

pub const UID_FILE: &str = "/var/log/uid";
pub const GEN5_POWER_TTY: &str = "/var/log/gen5_power.csv";
pub const GEN5_UART_TTY: &str = "/var/log/gen5_uart.csv";
pub const USB_MAPPING_FILE: &str = "/var/log/usb_mapping.csv";
pub const GEN5_MAPPING_FILE: &str = "/var/log/gen5_mapping.csv";
pub const TEMP_FILE_PATH: &str = "/var/log/temp.csv";
pub const LOG_PATH: &str = "/var/log/dev-controller.log";
pub const LOGIN_PATH: &str = "/etc/config/login.cfg";
pub const GEN5_IPL_LOG_PREFIX: &str = "/var/log/gen5-ipl";
pub const GEN4_IPL_LOG_PREFIX: &str = "/var/log/gen4-ipl";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub server_ip: String,
    pub http_port: u16,
    pub ws_port: u16,
    pub gen: Generation,
    pub bind_port: u16,
    pub iface_name: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server_ip: "127.0.0.1".into(),
            http_port: 5000,
            ws_port: 5002,
            gen: Generation::Gen5,
            bind_port: 8888,
            iface_name: "eth0".into(),
        }
    }
}

impl AppConfig {
    pub fn load_legacy_cfg(path: &str) -> AppResult<Self> {
        let mut cfg = Self::default();
        let text = fs::read_to_string(path)
            .map_err(|e| AppError::Msg(format!("failed to read config {path}: {e}")))?;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let key = k.trim();
                let val = v.trim();
                match key {
                    "server_ip" => cfg.server_ip = val.to_string(),
                    "http_port" => cfg.http_port = val.parse().unwrap_or(5000),
                    "ws_port" => cfg.ws_port = val.parse().unwrap_or(5002),
                    "gen" => {
                        let g = val.parse::<u8>().unwrap_or(5);
                        cfg.gen = Generation::from_int(g).unwrap_or(Generation::Gen5);
                    }
                    _ => {}
                }
            }
        }

        cfg.iface_name = match cfg.gen {
            Generation::Gen3 | Generation::Gen4 => "eth0".into(),
            Generation::Gen5 => "eno1".into(),
        };

        Ok(cfg)
    }

    pub fn ensure_parent(path: &str) -> AppResult<()> {
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(parent).map_err(AppError::Io)?;
        }
        Ok(())
    }
}
