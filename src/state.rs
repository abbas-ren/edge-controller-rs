use crate::{
    config::*,
    error::{AppError, AppResult},
    jobs::Jobs,
    models::{RegistrationPayload, RelayInventory},
    observability::metrics::global_metrics,
    relay::{RelayController, RelaySelector},
    store::{self, Gen5Mappings, UartMappings, UsbMappings},
    FeatureFlags,
};

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

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
    pub uart_map: RwLock<UartMappings>,
    pub rtos_sessions: Mutex<HashMap<String, RtosSession>>,
    pub relay: RelayController,
    pub(crate) deletion_requested: AtomicBool,
    pub client: reqwest::Client,
    pub jobs: Jobs,
    pub api_token: RwLock<Option<String>>,
    pub features: FeatureFlags,
    pub registration_done: AtomicBool,
}

impl AppState {
    pub async fn new(
        cfg: AppConfig,
        board_mac: String,
        board_ip: String,
        features: FeatureFlags,
        relay_selector: Option<RelaySelector>,
        api_token: Option<String>,
    ) -> AppResult<Arc<Self>> {
        cfg.validate()?;
        store::checked_mac(&board_mac)?;

        board_ip
            .parse::<std::net::Ipv4Addr>()
            .map_err(|_| AppError::Msg("controller interface IP is not IPv4".into()))?;

        if api_token.as_ref().is_some_and(|token| {
            !(32..=256).contains(&token.len()) || !token.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            tracing::error!(
                token_length = api_token.as_ref().map_or(0, |token| token.len()),
                "invalid DEV_CONTROLLER_TOKEN rejected"
            );
            return Err(AppError::Msg(
                "DEV_CONTROLLER_TOKEN must be 32..256 visible ASCII characters".into(),
            ));
        }

        let paths = crate::control::paths();
        let (usb_map, gen5_map, uart_map, uid) = tokio::task::spawn_blocking(move || {
            tracing::info!("loading persisted controller state");
            let (usb_map, gen5_map) = store::load_mappings(&paths.usb, &paths.gen5)?;
            let uart_map = store::load_uart_mappings(&paths.uart)?;

            let uid = store::load_uid(&paths.uid)?;

            tracing::debug!(
                usb_map_count = usb_map.len(),
                gen5_map_count = gen5_map.len(),
                saved_uid_present = uid.is_some(),
                "persisted controller state loaded"
            );

            if usb_map.is_empty() && gen5_map.is_empty() {
                tracing::info!(
                    usb_mapping_path = %paths.usb.display(),
                    gen5_mapping_path = %paths.gen5.display(),
                    "mapping files are empty or absent; starting from an empty persisted state"
                );
            }

            Ok::<_, AppError>((usb_map, gen5_map, uart_map, uid))
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

        let relay = RelayController::new(relay_selector)?;
        let state = Arc::new(Self {
            cfg,
            controller: RwLock::new(ControllerInfo {
                board_mac: board_mac.clone(),
                board_ip: board_ip.clone(),
                uid,
            }),
            usb_map: RwLock::new(usb_map),
            gen5_map: RwLock::new(gen5_map),
            uart_map: RwLock::new(uart_map),
            rtos_sessions: Mutex::new(HashMap::new()),
            relay,
            deletion_requested: AtomicBool::new(false),
            client,
            jobs: Jobs::default(),
            api_token: RwLock::new(api_token),
            features,
            registration_done: AtomicBool::new(false),
        });

        global_metrics().set_mapping_counts(
            state.usb_map.read().await.len(),
            state.gen5_map.read().await.len(),
        );

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

        let path = crate::control::paths().uid;
        tracing::info!(uid = %uid, path = %path.display(), "persisting controller UID to state store");

        // No .await between commit and publication.
        store::atomic_replace(&path, uid.as_bytes())?;
        controller.uid = Some(uid.to_owned());

        tracing::debug!(uid = %uid, "controller UID written and published in memory");
        Ok(())
    }

    pub async fn clear_uid(&self) -> AppResult<()> {
        let mut controller = self.controller.write().await;
        let path = crate::control::paths().uid;

        tracing::warn!(path = %path.display(), "clearing persisted controller UID");
        store::remove_committed(&path)?;
        controller.uid = None;

        tracing::info!(path = %path.display(), "controller UID removed from persisted state and memory");
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

    pub fn try_begin_registration(&self) -> bool {
        !self.registration_done.swap(true, Ordering::AcqRel)
    }

    pub fn request_deletion(&self) {
        self.deletion_requested.store(true, Ordering::Release);
    }

    pub fn deletion_requested(&self) -> bool {
        self.deletion_requested.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::{AppState, ControllerInfo};
    use crate::{
        config::AppConfig, jobs::Jobs, models::Generation, relay::RelayController, FeatureFlags,
    };
    use std::{collections::HashMap, sync::atomic::AtomicBool};
    use tokio::sync::{Mutex, RwLock};

    #[test]
    fn registration_guard_only_allows_one_transition() {
        let guard = AtomicBool::new(false);

        assert!(std::sync::atomic::AtomicBool::compare_exchange(
            &guard,
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_ok());
        assert!(!std::sync::atomic::AtomicBool::compare_exchange(
            &guard,
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_ok());
    }

    #[test]
    fn deletion_signal_does_not_begin_service_shutdown() {
        let state = AppState {
            cfg: AppConfig {
                server_ip: "127.0.0.1".into(),
                http_port: 1,
                ws_port: 1,
                gen: Generation::Gen4,
                bind_port: 8888,
                iface_name: "eth0".into(),
            },
            controller: RwLock::new(ControllerInfo {
                board_mac: "001122334455".into(),
                board_ip: "127.0.0.1".into(),
                uid: None,
            }),
            usb_map: RwLock::new(HashMap::new()),
            gen5_map: RwLock::new(HashMap::new()),
            uart_map: RwLock::new(HashMap::new()),
            rtos_sessions: Mutex::new(HashMap::new()),
            relay: RelayController::new(None).unwrap(),
            deletion_requested: AtomicBool::new(false),
            client: reqwest::Client::new(),
            jobs: Jobs::default(),
            api_token: RwLock::new(None),
            features: FeatureFlags::default(),
            registration_done: AtomicBool::new(false),
        };

        state.request_deletion();

        assert!(state.deletion_requested());
        assert!(!state.jobs.is_closing());
    }
}
