use crate::{
    config::*,
    error::{AppError, AppResult},
    hardware::HardwarePolicy,
    jobs::Jobs,
    models::{RegistrationPayload, RelayInventory},
    relay::RelayController,
    store::{self, Gen5Mappings, UsbMappings},
};

use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

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
    pub file: tempfile::NamedTempFile,
    pub stop: Arc<std::sync::atomic::AtomicBool>,
    pub handle: Option<tokio::task::JoinHandle<AppResult<()>>>,
}

pub struct AppState {
    pub cfg: AppConfig,
    pub controller: RwLock<ControllerInfo>,
    pub usb_map: RwLock<UsbMappings>,
    pub gen5_map: RwLock<Gen5Mappings>,
    pub rtos_sessions: Mutex<HashMap<String, RtosSession>>,
    pub relay: RelayController,
    pub reboot: RwLock<bool>,
    pub client: reqwest::Client,
    pub jobs: Jobs,
    pub api_token: Option<String>,
    pub hardware: HardwarePolicy,
}

impl AppState {
    pub async fn new(cfg: AppConfig, board_mac: String, board_ip: String) -> AppResult<Arc<Self>> {
        cfg.validate()?;
        store::checked_mac(&board_mac)?;

        board_ip
            .parse::<std::net::Ipv4Addr>()
            .map_err(|_| AppError::Msg("controller interface IP is not IPv4".into()))?;

        let api_token = configured_api_token()?;
        let hardware_path = configured_hardware_path()?;

        if api_token.as_ref().is_some_and(|token| {
            !(32..=256).contains(&token.len()) || !token.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            tracing::error!(token_length = api_token.as_ref().map_or(0, |token| token.len()), "invalid DEV_CONTROLLER_TOKEN rejected");
            return Err(AppError::Msg(
                "DEV_CONTROLLER_TOKEN must be 32..256 visible ASCII characters".into(),
            ));
        }

        if !Path::new(&hardware_path).is_absolute() {
            tracing::error!(hardware_path = %hardware_path, "invalid hardware policy path rejected");
            return Err(AppError::Msg(
                "DEV_CONTROLLER_HARDWARE must be an absolute pathname".into(),
            ));
        }

        let (hardware, usb_map, gen5_map, uid) = tokio::task::spawn_blocking(move || {
            tracing::info!(hardware_path = %hardware_path, "loading hardware policy and persisted state");

            let hardware = HardwarePolicy::load(&hardware_path)?;
            tracing::info!(
                board_count = hardware.boards.len(),
                hardware_path = %hardware_path,
                "hardware policy loaded and validated"
            );

            let (usb_map, gen5_map) = store::load_mappings(
                Path::new(USB_MAPPING_FILE),
                Path::new(GEN5_MAPPING_FILE),
                &hardware,
            )?;

            let uid = store::load_uid(Path::new(UID_FILE))?;

            tracing::debug!(
                usb_map_count = usb_map.len(),
                gen5_map_count = gen5_map.len(),
                saved_uid_present = uid.is_some(),
                "persisted controller state loaded"
            );

            if usb_map.is_empty() && gen5_map.is_empty() {
                tracing::info!(
                    usb_mapping_path = %USB_MAPPING_FILE,
                    gen5_mapping_path = %GEN5_MAPPING_FILE,
                    "mapping files are empty or absent; starting from an empty persisted state"
                );
            }

            Ok::<_, AppError>((hardware, usb_map, gen5_map, uid))
        })
        .await
        .map_err(|error| AppError::Msg(format!("state-loading worker failed: {error}")))??;

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|error| {
                AppError::Msg(format!("HTTP client initialization failed: {error}"))
            })?;

        let state = Arc::new(Self {
            cfg,
            controller: RwLock::new(ControllerInfo {
                board_mac: board_mac.clone(),
                board_ip: board_ip.clone(),
                uid,
            }),
            usb_map: RwLock::new(usb_map),
            gen5_map: RwLock::new(gen5_map),
            rtos_sessions: Mutex::new(HashMap::new()),
            relay: RelayController::new(),
            reboot: RwLock::new(false),
            client,
            jobs: Jobs::default(),
            api_token,
            hardware,
        });

        let controller_uid = state.controller.read().await.uid.clone();

        tracing::info!(
            board_mac = %board_mac,
            board_ip = %board_ip,
            controller_uid = ?controller_uid,
            "controller application state initialized"
        );

        Ok(state)
    }

    pub async fn save_uid(&self, uid: &str) -> AppResult<()> {
        store::validate_uid(uid)?;
        let mut controller = self.controller.write().await;

        tracing::info!(uid = %uid, path = %UID_FILE, "persisting controller UID to state store");

        // No .await between commit and publication.
        store::atomic_replace(Path::new(UID_FILE), uid.as_bytes())?;
        controller.uid = Some(uid.to_owned());

        tracing::debug!(uid = %uid, "controller UID written and published in memory");
        Ok(())
    }

    pub async fn clear_uid(&self) -> AppResult<()> {
        let mut controller = self.controller.write().await;

        tracing::warn!(path = %UID_FILE, "clearing persisted controller UID");
        store::remove_committed(Path::new(UID_FILE))?;
        controller.uid = None;

        tracing::info!(path = %UID_FILE, "controller UID removed from persisted state and memory");
        Ok(())
    }

    pub async fn registration_payload(&self, relays: Vec<RelayInventory>) -> RegistrationPayload {
        let controller = self.controller.read().await;
        let relay_count = relays.len();

        tracing::debug!(
            mac = %controller.board_mac,
            ip = %controller.board_ip,
            uid = ?controller.uid,
            relay_count,
            "building backend registration payload"
        );

        RegistrationPayload {
            mac_address: controller.board_mac.clone(),
            ip_address: controller.board_ip.clone(),
            device_family: format!("Gen{}", self.cfg.gen.as_int()),
            relays,
            uid: controller.uid.clone(),
        }
    }
}

/// Legacy normalization helper, retained for existing internal callers.
///
/// External values should use store::checked_mac instead. Keeping this helper in
/// the state module documents the earlier normalization contract while avoiding
/// new call sites that might reintroduce inconsistent MAC parsing.
#[allow(dead_code)]
pub fn normalize_mac(value: &str) -> String {
    value
        .to_ascii_lowercase()
        .chars()
        .filter(|character| !matches!(character, ':' | '-' | ' '))
        .collect()
}
