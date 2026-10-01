//! Strict parsing of the legacy key=value configuration.
//!
//! Rules:
//! - Required: server_ip, http_port, ws_port, gen.
//! - Optional: iface_name, bind_port.
//! - Blank lines and whole-line '#' / ';' comments are permitted.
//! - Duplicate and unknown keys are errors.
//! - Inline comments, quoting, and environment interpolation are unsupported.
//! - Whitespace surrounding keys and values is ignored.

use crate::{
    error::{AppError, AppResult},
    models::Generation,
};

use serde::Serialize;
use std::{collections::BTreeMap, net::Ipv4Addr, path::Path};

pub const UID_FILE: &str = "/var/log/uid";
pub const GEN5_POWER_TTY: &str = "/var/log/gen5_power.csv";
pub const GEN5_UART_TTY: &str = "/var/log/gen5_uart.csv";
pub const USB_MAPPING_FILE: &str = "/var/log/usb_mapping.csv";
pub const GEN5_MAPPING_FILE: &str = "/var/log/gen5_mapping.csv";

const MAX_CONFIG_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct AppConfig {
    pub server_ip: String,
    pub http_port: u16,
    pub ws_port: u16,
    pub gen: Generation,
    pub bind_port: u16,
    pub iface_name: String,
}

fn invalid(message: impl Into<String>) -> AppError {
    AppError::Msg(message.into())
}

fn port(value: &str, field: &str) -> AppResult<u16> {
    // Listener ports are part of the device's network contract and must be
    // explicit: a zero port would silently break startup or upstream routing.
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid(format!("{field} must be a decimal port number")));
    }

    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| invalid(format!("{field} must be between 1 and 65535")))
}

pub fn validate_interface(value: &str) -> AppResult<()> {
    // Linux IFNAMSIZ is 16, including the terminating NUL.
    if value.is_empty()
        || value.len() > 15
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(invalid(
            "iface_name must be 1..15 ASCII letters/digits or '_', '-', '.'",
        ));
    }

    Ok(())
}

impl AppConfig {
    pub fn load_legacy_cfg(path: &str) -> AppResult<Self> {
        let text = crate::store::read_required_text(Path::new(path), MAX_CONFIG_BYTES)?;

        Self::parse(&text)
            .map_err(|error| invalid(format!("invalid configuration {}: {error}", path)))
    }

    pub fn parse(text: &str) -> AppResult<Self> {
        if text.len() > MAX_CONFIG_BYTES {
            return Err(invalid("configuration exceeds 16 KiB"));
        }

        let mut fields = BTreeMap::new();

        for (index, line) in text.lines().enumerate() {
            let line = line.trim();

            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }

            if line.len() > 1024 {
                return Err(invalid(format!(
                    "configuration line {} exceeds 1024 bytes",
                    index + 1
                )));
            }

            let (key, value) = line.split_once('=').ok_or_else(|| {
                invalid(format!(
                    "configuration line {} must contain key=value",
                    index + 1
                ))
            })?;

            let key = key.trim();
            let value = value.trim();

            if !matches!(
                key,
                "server_ip" | "http_port" | "ws_port" | "gen" | "bind_port" | "iface_name"
            ) {
                return Err(invalid(format!(
                    "unknown configuration key on line {}: {key:?}",
                    index + 1
                )));
            }

            if value.is_empty() || value.chars().any(char::is_control) {
                return Err(invalid(format!("empty or invalid value for {key}")));
            }

            if fields.insert(key, value).is_some() {
                return Err(invalid(format!("duplicate configuration key: {key}")));
            }
        }

        let required = |key: &str| -> AppResult<&str> {
            fields
                .get(key)
                .copied()
                .ok_or_else(|| invalid(format!("missing configuration key: {key}")))
        };

        tracing::debug!(
            entries = fields.len(),
            "legacy configuration parsed; validating required values"
        );

        let gen = match required("gen")? {
            "3" => Generation::Gen3,
            "4" => Generation::Gen4,
            "5" => Generation::Gen5,
            _ => return Err(invalid("gen must be exactly 3, 4, or 5")),
        };

        let cfg = Self {
            server_ip: required("server_ip")?.to_owned(),
            http_port: port(required("http_port")?, "http_port")?,
            ws_port: port(required("ws_port")?, "ws_port")?,
            gen,
            bind_port: port(
                fields.get("bind_port").copied().unwrap_or("8888"),
                "bind_port",
            )?,
            iface_name: fields
                .get("iface_name")
                .copied()
                .unwrap_or("eth0")
                .to_owned(),
        };

        cfg.validate()?;
        tracing::info!(
            server_ip = %cfg.server_ip,
            http_port = cfg.http_port,
            ws_port = cfg.ws_port,
            generation = cfg.gen.as_int(),
            bind_port = cfg.bind_port,
            iface_name = %cfg.iface_name,
            "application configuration validated"
        );
        Ok(cfg)
    }

    /// Revalidate after programmatic or environment-based overrides.
    pub fn validate(&self) -> AppResult<()> {
        let address = self
            .server_ip
            .parse::<Ipv4Addr>()
            .map_err(|_| invalid("server_ip must be an IPv4 address without a scheme or port"))?;

        if address.is_unspecified() || address.is_multicast() || address == Ipv4Addr::BROADCAST {
            return Err(invalid("server_ip is not a valid backend destination"));
        }

        if self.http_port == 0 || self.ws_port == 0 || self.bind_port == 0 {
            return Err(invalid("configured ports must not be zero"));
        }

        validate_interface(&self.iface_name)
    }
}

/// Read an optional environment variable without treating invalid UTF-8
/// as if the variable were absent.
pub fn optional_environment(name: &str) -> AppResult<Option<String>> {
    match std::env::var(name) {
        Ok(value) => {
            tracing::debug!(
                variable = name,
                value_length = value.len(),
                "environment variable set; using explicit override"
            );
            Ok(Some(value))
        }
        Err(std::env::VarError::NotPresent) => {
            tracing::debug!(
                variable = name,
                "environment variable not set; falling back to default behavior"
            );
            Ok(None)
        }
        Err(std::env::VarError::NotUnicode(_)) => {
            tracing::error!(variable = name, "environment variable is not valid UTF-8");
            Err(AppError::Msg(format!("{name} must be valid UTF-8")))
        }
    }
}

pub fn configured_api_token() -> AppResult<Option<String>> {
    let token = optional_environment("DEV_CONTROLLER_TOKEN")?;

    if token.as_ref().is_some_and(|value| {
        !(32..=256).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_graphic())
    }) {
        return Err(AppError::Msg(
            "DEV_CONTROLLER_TOKEN must contain 32..256 visible ASCII characters".into(),
        ));
    }

    Ok(token)
}

pub fn configured_hardware_path() -> AppResult<String> {
    let path = optional_environment("DEV_CONTROLLER_HARDWARE")?.unwrap_or_else(|| {
        tracing::info!(
            default_path = "/etc/dev-controller/hardware.json",
            "DEV_CONTROLLER_HARDWARE unset; using default hardware policy path"
        );
        "/etc/dev-controller/hardware.json".into()
    });

    if !Path::new(&path).is_absolute() {
        tracing::error!(
            configured_path = %path,
            "DEV_CONTROLLER_HARDWARE is not absolute; rejecting invalid path"
        );
        return Err(AppError::Msg(
            "DEV_CONTROLLER_HARDWARE must be an absolute pathname".into(),
        ));
    }

    tracing::info!(hardware_path = %path, "hardware policy path selected");
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "server_ip=127.0.0.1\nhttp_port=5000\nws_port=5002\ngen=4\n";

    #[test]
    fn parses_legacy_configuration_and_rpi_defaults() {
        let cfg = AppConfig::parse(VALID).unwrap();
        assert_eq!(cfg.http_port, 5000);
        assert_eq!(cfg.iface_name, "eth0");
        assert_eq!(cfg.bind_port, 8888);
        assert_eq!(cfg.gen, Generation::Gen4);
    }

    #[test]
    fn accepts_comments_crlf_and_explicit_optional_keys() {
        let text = format!("# comment\r\n{VALID}\r\niface_name = wlan0\r\nbind_port=9999\r\n");
        let cfg = AppConfig::parse(&text).unwrap();
        assert_eq!(cfg.iface_name, "wlan0");
        assert_eq!(cfg.bind_port, 9999);
    }

    #[test]
    fn rejects_duplicate_unknown_and_missing_keys() {
        assert!(AppConfig::parse(&format!("{VALID}gen=5\n")).is_err());
        assert!(AppConfig::parse(&format!("{VALID}typo=1\n")).is_err());
        assert!(AppConfig::parse("gen=4\n").is_err());
    }

    #[test]
    fn rejects_invalid_ports_and_generations() {
        for value in ["0", "65536", "-1", "+80", "80 # comment", "\"80\""] {
            assert!(AppConfig::parse(
                &VALID.replace("http_port=5000", &format!("http_port={value}"))
            )
            .is_err());
        }

        for value in ["0", "6", "04", "gen4"] {
            assert!(AppConfig::parse(&VALID.replace("gen=4", &format!("gen={value}"))).is_err());
        }
    }

    #[test]
    fn rejects_invalid_backend_and_interface_values() {
        for address in ["http://127.0.0.1", "0.0.0.0", "224.0.0.1", "host/name"] {
            assert!(AppConfig::parse(&VALID.replace("127.0.0.1", address)).is_err());
        }

        for interface in ["", "../eth0", "a b", "abcdefghijklmnop"] {
            assert!(validate_interface(interface).is_err());
        }
    }
}
