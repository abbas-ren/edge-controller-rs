use std::{
    path::{Path, PathBuf},
    sync::{OnceLock, RwLock},
};

use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    config::{
        GEN5_MAPPING_FILE, GEN5_POWER_TTY, GEN5_UART_TTY, UART_MAPPING_FILE, UID_FILE,
        USB_MAPPING_FILE,
    },
    error::{AppError, AppResult},
};

pub const CONTROL_FILE: &str = "/var/lib/dev-controller/admin-control.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MappingPaths {
    pub uid: PathBuf,
    pub usb: PathBuf,
    pub gen5: PathBuf,
    pub uart: PathBuf,
    pub gen5_uart_inventory: PathBuf,
    pub gen5_power_inventory: PathBuf,
}

impl Default for MappingPaths {
    fn default() -> Self {
        Self {
            uid: UID_FILE.into(),
            usb: USB_MAPPING_FILE.into(),
            gen5: GEN5_MAPPING_FILE.into(),
            uart: UART_MAPPING_FILE.into(),
            gen5_uart_inventory: GEN5_UART_TTY.into(),
            gen5_power_inventory: GEN5_POWER_TTY.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ControlSettings {
    pub config_path: String,
    pub bind_address: String,
    pub bind_port: u16,
    pub metrics_port: u16,
    pub interface: String,
    pub server_ip: String,
    pub http_port: u16,
    pub ws_port: u16,
    pub generation: u8,
    pub enable_gen3: bool,
    pub enable_gen4: bool,
    pub enable_gen5: bool,
    pub enable_rtos: bool,
    pub relay_serial_number: Option<String>,
    pub relay_vid_pid: Option<String>,
    pub log_level: String,
    pub log_file: Option<String>,
    pub log_network: bool,
    pub log_stream: bool,
    pub auth_enabled: bool,
    pub api_token: Option<String>,
    pub paths: MappingPaths,
    pub restart_pending: bool,
}

impl Default for ControlSettings {
    fn default() -> Self {
        Self {
            config_path: crate::constants::DEFAULT_CONFIG_PATH.to_owned(),
            bind_address: "0.0.0.0".to_owned(),
            bind_port: crate::constants::DEFAULT_BIND_PORT,
            metrics_port: crate::constants::DEFAULT_METRICS_PORT,
            interface: "eth0".to_owned(),
            server_ip: "127.0.0.1".to_owned(),
            http_port: 5000,
            ws_port: 5002,
            generation: 4,
            enable_gen3: false,
            enable_gen4: false,
            enable_gen5: false,
            enable_rtos: false,
            relay_serial_number: None,
            relay_vid_pid: None,
            log_level: "info".to_owned(),
            log_file: None,
            log_network: false,
            log_stream: false,
            auth_enabled: false,
            api_token: None,
            paths: MappingPaths::default(),
            restart_pending: false,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ControlPatch {
    pub config_path: Option<String>,
    pub bind_address: Option<String>,
    pub bind_port: Option<u16>,
    pub metrics_port: Option<u16>,
    pub interface: Option<String>,
    pub server_ip: Option<String>,
    pub http_port: Option<u16>,
    pub ws_port: Option<u16>,
    pub generation: Option<u8>,
    pub enable_gen3: Option<bool>,
    pub enable_gen4: Option<bool>,
    pub enable_gen5: Option<bool>,
    pub enable_rtos: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_option")]
    pub relay_serial_number: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_optional_option")]
    pub relay_vid_pid: Option<Option<String>>,
    pub log_level: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_option")]
    pub log_file: Option<Option<String>>,
    pub log_network: Option<bool>,
    pub log_stream: Option<bool>,
    pub auth_enabled: Option<bool>,
    pub api_token: Option<String>,
    pub paths: Option<MappingPaths>,
}

fn deserialize_optional_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

static SETTINGS: OnceLock<RwLock<ControlSettings>> = OnceLock::new();
static ACTIVE_PATHS: OnceLock<MappingPaths> = OnceLock::new();

pub fn load(path: &Path) -> AppResult<Option<ControlSettings>> {
    let contents = match std::fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AppError::Msg(format!(
                "cannot read admin control file {}: {error}",
                path.display()
            )));
        }
    };
    if contents.len() > 64 * 1024 {
        return Err(AppError::Msg("admin control file exceeds 64 KiB".into()));
    }
    let settings = serde_json::from_slice::<ControlSettings>(&contents)
        .map_err(|error| AppError::Msg(format!("invalid admin control file: {error}")))?;
    validate(&settings)?;
    Ok(Some(settings))
}

pub fn install(mut settings: ControlSettings) -> AppResult<()> {
    validate(&settings)?;
    settings.restart_pending = false;
    ACTIVE_PATHS
        .set(settings.paths.clone())
        .map_err(|_| AppError::Msg("active mapping paths were already initialized".into()))?;
    SETTINGS
        .set(RwLock::new(settings))
        .map_err(|_| AppError::Msg("admin control state was already initialized".into()))
}

pub fn current() -> ControlSettings {
    SETTINGS
        .get()
        .map(|settings| {
            settings
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        })
        .unwrap_or_default()
}

pub fn paths() -> MappingPaths {
    ACTIVE_PATHS.get().cloned().unwrap_or_default()
}

pub fn apply(patch: ControlPatch) -> AppResult<ControlSettings> {
    let settings = SETTINGS
        .get()
        .ok_or_else(|| AppError::Msg("admin control state is not initialized".into()))?;
    let mut next = settings
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let mut restart_required = false;

    macro_rules! replace_restart {
        ($field:ident) => {
            if let Some(value) = patch.$field {
                if next.$field != value {
                    next.$field = value;
                    restart_required = true;
                }
            }
        };
    }

    replace_restart!(config_path);
    replace_restart!(bind_address);
    replace_restart!(bind_port);
    replace_restart!(metrics_port);
    replace_restart!(interface);
    replace_restart!(server_ip);
    replace_restart!(http_port);
    replace_restart!(ws_port);
    replace_restart!(generation);
    replace_restart!(enable_gen3);
    replace_restart!(enable_gen4);
    replace_restart!(enable_gen5);
    replace_restart!(enable_rtos);
    replace_restart!(relay_serial_number);
    replace_restart!(relay_vid_pid);
    replace_restart!(log_file);
    replace_restart!(log_network);
    replace_restart!(log_stream);

    if let Some(level) = patch.log_level {
        next.log_level = level;
    }
    if let Some(enabled) = patch.auth_enabled {
        next.auth_enabled = enabled;
    }
    if let Some(token) = patch.api_token {
        next.api_token = Some(token);
    }
    if !next.auth_enabled {
        next.api_token = None;
    }
    if let Some(paths) = patch.paths {
        if next.paths.uid != paths.uid
            || next.paths.usb != paths.usb
            || next.paths.gen5 != paths.gen5
            || next.paths.uart != paths.uart
            || next.paths.gen5_uart_inventory != paths.gen5_uart_inventory
            || next.paths.gen5_power_inventory != paths.gen5_power_inventory
        {
            next.paths = paths;
            restart_required = true;
        }
    }
    next.restart_pending |= restart_required;
    validate(&next)?;
    persist(Path::new(CONTROL_FILE), &next)?;
    *settings.write().unwrap_or_else(|error| error.into_inner()) = next.clone();
    Ok(next)
}

pub fn persist(path: &Path, settings: &ControlSettings) -> AppResult<()> {
    validate(settings)?;
    let contents = serde_json::to_vec_pretty(settings)
        .map_err(|error| AppError::Msg(format!("cannot encode admin control state: {error}")))?;
    crate::store::atomic_replace(path, &contents)
}

pub fn validate(settings: &ControlSettings) -> AppResult<()> {
    if settings.bind_port == 0
        || settings.metrics_port == 0
        || settings.http_port == 0
        || settings.ws_port == 0
    {
        return Err(AppError::Msg(
            "control-plane ports must be between 1 and 65535".into(),
        ));
    }
    settings
        .bind_address
        .parse::<std::net::IpAddr>()
        .map_err(|_| AppError::Msg("bindAddress must be an IP address".into()))?;
    settings
        .server_ip
        .parse::<std::net::Ipv4Addr>()
        .map_err(|_| AppError::Msg("serverIp must be an IPv4 address".into()))?;
    crate::config::validate_interface(&settings.interface)?;
    if !matches!(settings.generation, 3..=5) {
        return Err(AppError::Msg("generation must be 3, 4, or 5".into()));
    }
    if !matches!(
        settings.log_level.as_str(),
        "trace" | "debug" | "info" | "warn" | "error" | "off"
    ) {
        return Err(AppError::Msg("unsupported log level".into()));
    }
    if settings.auth_enabled
        && settings.api_token.as_ref().is_none_or(|token| {
            !(32..=256).contains(&token.len()) || !token.bytes().all(|byte| byte.is_ascii_graphic())
        })
    {
        return Err(AppError::Msg(
            "auth requires a 32..256 character visible ASCII token".into(),
        ));
    }
    if (settings.enable_gen3 || settings.enable_gen4) && settings.relay_vid_pid.is_none() {
        return Err(AppError::Msg(
            "Gen3/Gen4 requires a relay VID:PID selector".into(),
        ));
    }
    if settings.relay_serial_number.is_some() && settings.relay_vid_pid.is_none() {
        return Err(AppError::Msg(
            "relaySerialNumber requires relayVidPid".into(),
        ));
    }
    if let Some(vid_pid) = &settings.relay_vid_pid {
        let valid = vid_pid.len() == 9
            && vid_pid.as_bytes()[4] == b':'
            && vid_pid
                .chars()
                .enumerate()
                .all(|(index, value)| index == 4 || value.is_ascii_hexdigit());
        if !valid {
            return Err(AppError::Msg(
                "relayVidPid must use hexadecimal VVVV:PPPP".into(),
            ));
        }
    }
    validate_path("configPath", Path::new(&settings.config_path), false)?;
    if let Some(log_file) = &settings.log_file {
        validate_path("logFile", Path::new(log_file), true)?;
    }
    validate_path("paths.uid", &settings.paths.uid, true)?;
    validate_path("paths.usb", &settings.paths.usb, true)?;
    validate_path("paths.gen5", &settings.paths.gen5, true)?;
    validate_path("paths.uart", &settings.paths.uart, true)?;
    validate_path(
        "paths.gen5UartInventory",
        &settings.paths.gen5_uart_inventory,
        true,
    )?;
    validate_path(
        "paths.gen5PowerInventory",
        &settings.paths.gen5_power_inventory,
        true,
    )?;
    Ok(())
}

fn validate_path(name: &str, path: &Path, require_writable_root: bool) -> AppResult<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(AppError::Msg(format!(
            "{name} must be an absolute normalized path"
        )));
    }
    if require_writable_root
        && !["/var/log", "/var/lib/dev-controller", "/etc/log"]
            .iter()
            .any(|root| path.starts_with(root))
    {
        return Err(AppError::Msg(format!(
            "{name} must remain under /var/log, /var/lib/dev-controller, or /etc/log"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_auth_disabled_and_use_legacy_mapping_paths() {
        let settings = ControlSettings::default();
        assert!(!settings.auth_enabled);
        assert!(settings.api_token.is_none());
        assert_eq!(settings.paths.uart, PathBuf::from(UART_MAPPING_FILE));
        validate(&settings).unwrap();
    }

    #[test]
    fn rejects_paths_outside_service_writable_roots() {
        let mut settings = ControlSettings::default();
        settings.paths.usb = PathBuf::from("/home/user/mapping.csv");
        assert!(validate(&settings).is_err());
    }

    #[test]
    fn authentication_requires_a_strong_explicit_token() {
        let mut settings = ControlSettings {
            auth_enabled: true,
            api_token: Some("short".into()),
            ..ControlSettings::default()
        };
        assert!(validate(&settings).is_err());
        settings.api_token = Some("a".repeat(32));
        validate(&settings).unwrap();
    }

    #[test]
    fn control_patch_distinguishes_omitted_and_cleared_optional_values() {
        let omitted = serde_json::from_value::<ControlPatch>(serde_json::json!({})).unwrap();
        assert_eq!(omitted.relay_serial_number, None);
        assert_eq!(omitted.relay_vid_pid, None);
        assert_eq!(omitted.log_file, None);

        let cleared = serde_json::from_value::<ControlPatch>(serde_json::json!({
            "relaySerialNumber": null,
            "relayVidPid": null,
            "logFile": null
        }))
        .unwrap();
        assert_eq!(cleared.relay_serial_number, Some(None));
        assert_eq!(cleared.relay_vid_pid, Some(None));
        assert_eq!(cleared.log_file, Some(None));
    }
}
