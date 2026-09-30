use crate::config::*;
use crate::error::{AppError, AppResult};
use crate::models::RegistrationPayload;
use crate::relay::RelayController;
use crate::usb::Gen5MapEntry;
use std::{collections::HashMap, fs, sync::Arc};
use tokio::sync::{Mutex, RwLock};

#[derive(Debug, Clone)]
pub struct ControllerInfo {
    pub board_mac: String,
    pub board_ip: String,
    pub uid: Option<String>,
}

#[derive(Debug)]
pub struct RtosSession {
    pub tty: String,
    /// Automatically removes the capture pathname when the session is dropped.
    pub file: tempfile::NamedTempFile,
    pub stop: Arc<std::sync::atomic::AtomicBool>,
    /// Preserve worker errors instead of silently discarding them.
    pub handle: Option<tokio::task::JoinHandle<AppResult<()>>>,
}

#[derive(Debug)]
pub struct AppState {
    pub cfg: AppConfig,
    pub controller: RwLock<ControllerInfo>,
    pub usb_map: RwLock<HashMap<(String, String, u8), String>>,
    pub gen5_map: RwLock<HashMap<String, Gen5MapEntry>>,
    pub rtos_sessions: Mutex<HashMap<String, RtosSession>>,
    pub relay: RelayController,
    pub reboot: RwLock<bool>,
    pub client: reqwest::Client,
    pub hardware: crate::hardware::HardwarePolicy,

    pub jobs: crate::jobs::Jobs,

    /// Optional bearer token. Required by main when listening beyond loopback.
    pub api_token: Option<String>,
}

impl AppState {
    pub async fn new(cfg: AppConfig, board_mac: String, board_ip: String) -> AppResult<Arc<Self>> {
        let api_token = std::env::var("DEV_CONTROLLER_TOKEN").ok();

        if api_token
            .as_ref()
            .is_some_and(|token| token.len() < 32 || !token.is_ascii())
        {
            return Err(AppError::Msg(
                "DEV_CONTROLLER_TOKEN must contain at least 32 ASCII characters".into(),
            ));
        }
        let hardware_path = std::env::var("DEV_CONTROLLER_HARDWARE")
            .unwrap_or_else(|_| "/etc/dev-controller/hardware.json".into());

        let hardware = crate::hardware::HardwarePolicy::load(&hardware_path)?;

        AppConfig::ensure_parent(UID_FILE)?;
        AppConfig::ensure_parent(USB_MAPPING_FILE)?;
        AppConfig::ensure_parent(GEN5_MAPPING_FILE)?;
        let uid = fs::read_to_string(UID_FILE)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let state = Arc::new(Self {
            cfg,
            controller: RwLock::new(ControllerInfo {
                board_mac,
                board_ip,
                uid,
            }),
            usb_map: RwLock::new(HashMap::new()),
            gen5_map: RwLock::new(HashMap::new()),
            rtos_sessions: Mutex::new(HashMap::new()),
            relay: RelayController::new(),
            reboot: RwLock::new(false),
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(15))
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .map_err(|e| AppError::Msg(format!("HTTP client initialization failed: {e}")))?,
            hardware,
            jobs: crate::jobs::Jobs::default(),
            api_token,
        });

        state.load_usb_mapping_file().await?;
        state.load_gen5_mapping_file().await?;
        Ok(state)
    }

    pub async fn save_uid(&self, uid: &str) -> AppResult<()> {
        fs::write(UID_FILE, uid).map_err(AppError::Io)?;
        self.controller.write().await.uid = Some(uid.to_string());
        Ok(())
    }

    pub async fn clear_uid(&self) -> AppResult<()> {
        let mut controller = self.controller.write().await;

        match tokio::fs::remove_file(UID_FILE).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(AppError::Io(error)),
        }

        controller.uid = None;
        Ok(())
    }

    pub async fn registration_payload(
        &self,
        relays: Vec<crate::models::RelayInventory>,
    ) -> RegistrationPayload {
        let c = self.controller.read().await;
        RegistrationPayload {
            mac_address: c.board_mac.clone(),
            ip_address: c.board_ip.clone(),
            device_family: format!("Gen{}", self.cfg.gen.as_int()),
            relays,
            uid: c.uid.clone(),
        }
    }

    pub(crate) async fn load_usb_mapping_file(&self) -> AppResult<()> {
        let text = fs::read_to_string(USB_MAPPING_FILE).unwrap_or_default();
        let mut map = self.usb_map.write().await;
        map.clear();

        for line in text.lines() {
            let parts: Vec<_> = line.split(',').map(str::trim).collect();
            if parts.len() == 4 {
                let tty = parts[0].to_string();
                let mac = normalize_mac(parts[1]);
                let serial = parts[2].to_string();
                let channel = parts[3].parse::<u8>().unwrap_or(0);
                map.insert((mac, serial, channel), tty);
            }
        }
        Ok(())
    }

    pub(crate) async fn load_gen5_mapping_file(&self) -> AppResult<()> {
        let text = fs::read_to_string(GEN5_MAPPING_FILE).unwrap_or_default();
        let mut map = self.gen5_map.write().await;
        map.clear();

        for line in text.lines() {
            let parts: Vec<_> = line.split(',').map(str::trim).collect();
            if parts.len() == 3 {
                map.insert(
                    normalize_mac(parts[2]),
                    Gen5MapEntry {
                        uart: parts[0].to_string(),
                        power: parts[1].to_string(),
                        mac: normalize_mac(parts[2]),
                    },
                );
            }
        }
        Ok(())
    }
}

pub fn normalize_mac(s: &str) -> String {
    s.to_ascii_lowercase()
        .chars()
        .filter(|c| *c != ':' && *c != '-' && *c != ' ')
        .collect()
}
