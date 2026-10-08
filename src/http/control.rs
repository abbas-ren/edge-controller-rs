use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::{
    control::{self, ControlPatch},
    state::AppState,
    store::{Gen5Mappings, UartMappings, UsbMappings},
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlActionRequest {
    action: ControlAction,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum ControlAction {
    Restart,
    ReloadMappings,
    ClearUsbMappings,
    ClearGen5Mappings,
    ClearUartMappings,
    ClearControllerUid,
}

pub async fn get(State(state): State<Arc<AppState>>) -> Response {
    Json(snapshot(&state).await).into_response()
}

pub async fn patch(
    State(state): State<Arc<AppState>>,
    Json(patch): Json<ControlPatch>,
) -> Response {
    let previous = control::current();
    let next = match control::apply(patch) {
        Ok(settings) => settings,
        Err(error) => return super::error_response(error.to_string(), StatusCode::BAD_REQUEST),
    };

    if previous.log_level != next.log_level {
        if let Err(error) = crate::logging::set_level(&next.log_level) {
            return super::error_response(error.to_string(), StatusCode::BAD_REQUEST);
        }
    }

    if previous.auth_enabled != next.auth_enabled || previous.api_token != next.api_token {
        *state.api_token.write().await =
            next.auth_enabled.then(|| next.api_token.clone()).flatten();
        tracing::warn!(
            auth_enabled = next.auth_enabled,
            "EdgeController API authentication mode changed by control plane"
        );
    }

    tracing::info!(
        restart_pending = next.restart_pending,
        "EdgeController control settings updated"
    );
    Json(snapshot(&state).await).into_response()
}

pub async fn action(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ControlActionRequest>,
) -> Response {
    let result = match request.action {
        ControlAction::Restart => {
            schedule_restart();
            return (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({
                    "accepted": true,
                    "action": "restart",
                    "service": "dev-con.service"
                })),
            )
                .into_response();
        }
        ControlAction::ReloadMappings => reload_mappings(&state).await,
        ControlAction::ClearUsbMappings => clear_usb_mappings(&state).await,
        ControlAction::ClearGen5Mappings => clear_gen5_mappings(&state).await,
        ControlAction::ClearUartMappings => clear_uart_mappings(&state).await,
        ControlAction::ClearControllerUid => state.clear_uid().await,
    };

    match result {
        Ok(()) => Json(serde_json::json!({
            "accepted": true,
            "action": format!("{:?}", request.action),
            "state": snapshot(&state).await
        }))
        .into_response(),
        Err(error) => super::error_response(error.to_string(), StatusCode::CONFLICT),
    }
}

async fn snapshot(state: &AppState) -> serde_json::Value {
    let settings = control::current();
    let gpio_header_pins = crate::gpio::HEADER_PINS
        .iter()
        .map(|(physical_pin, line_offset)| {
            serde_json::json!({
                "physicalPin": physical_pin,
                "lineOffset": line_offset
            })
        })
        .collect::<Vec<_>>();
    let active_token = state.api_token.read().await;
    let controller = state.controller.read().await;
    let active_paths = control::paths();
    serde_json::json!({
        "service": "edgecontroller",
        "version": env!("CARGO_PKG_VERSION"),
        "restartPending": settings.restart_pending,
        "authentication": {
            "enabled": active_token.is_some(),
            "tokenConfigured": active_token.is_some(),
            "defaultEnabled": false
        },
        "logging": {
            "level": crate::logging::current_level().unwrap_or_else(|_| settings.log_level.clone()),
            "file": settings.log_file,
            "networkDetails": settings.log_network,
            "streamDetails": settings.log_stream
        },
        "network": {
            "bindAddress": settings.bind_address,
            "bindPort": settings.bind_port,
            "metricsPort": settings.metrics_port,
            "interface": settings.interface,
            "farmControllerIp": settings.server_ip,
            "farmControllerHttpPort": settings.http_port,
            "farmControllerWebSocketPort": settings.ws_port,
            "boardIp": controller.board_ip,
            "boardMac": controller.board_mac
        },
        "hardware": {
            "generation": settings.generation,
            "raspberryPiModel": settings.raspberry_pi_model,
            "gpioHeaderProfile": {
                "model": settings.raspberry_pi_model,
                "chip": crate::gpio::default_device(settings.raspberry_pi_model),
                "pins": gpio_header_pins
            },
            "relaySerialNumber": settings.relay_serial_number,
            "relayVidPid": settings.relay_vid_pid
        },
        "features": {
            "active": {
                "gen3": state.features.gen3,
                "gen4": state.features.gen4,
                "gen5": state.features.gen5,
                "rtos": state.features.rtos
            },
            "staged": {
                "gen3": settings.enable_gen3,
                "gen4": settings.enable_gen4,
                "gen5": settings.enable_gen5,
                "rtos": settings.enable_rtos
            }
        },
        "paths": {
            "active": active_paths,
            "staged": settings.paths,
            "config": settings.config_path
        },
        "mappings": {
            "usb": state.usb_map.read().await.len(),
            "gen5": state.gen5_map.read().await.len(),
            "uart": state.uart_map.read().await.len(),
            "uidConfigured": controller.uid.is_some()
        },
        "capabilities": [
            "runtimeLogging", "apiAuthentication", "network", "features",
            "hardwareSelector", "mappingPaths", "mappingMaintenance", "restart"
        ],
        "constraints": {
            "authTokenLength": {"min": 32, "max": 256},
            "raspberryPiModels": [4, 5],
            "mappingRoots": ["/var/log", "/var/lib/dev-controller", "/etc/log"],
            "restartService": "dev-con.service"
        }
    })
}

async fn reload_mappings(state: &AppState) -> crate::error::AppResult<()> {
    let paths = control::paths();
    let (usb, gen5, uart) = tokio::task::spawn_blocking(move || {
        let (usb, gen5) = crate::store::load_mappings(&paths.usb, &paths.gen5)?;
        let uart = crate::store::load_uart_mappings(&paths.uart)?;
        Ok::<_, crate::error::AppError>((usb, gen5, uart))
    })
    .await
    .map_err(|error| {
        crate::error::AppError::Msg(format!("mapping reload worker failed: {error}"))
    })??;
    *state.usb_map.write().await = usb;
    *state.gen5_map.write().await = gen5;
    *state.uart_map.write().await = uart;
    Ok(())
}

async fn clear_usb_mappings(state: &AppState) -> crate::error::AppResult<()> {
    let mappings = UsbMappings::new();
    let contents = crate::store::encode_usb(&mappings)?;
    crate::store::atomic_replace(&control::paths().usb, contents.as_bytes())?;
    *state.usb_map.write().await = mappings;
    Ok(())
}

async fn clear_gen5_mappings(state: &AppState) -> crate::error::AppResult<()> {
    let mappings = Gen5Mappings::new();
    let contents = crate::store::encode_gen5(&mappings)?;
    crate::store::atomic_replace(&control::paths().gen5, contents.as_bytes())?;
    *state.gen5_map.write().await = mappings;
    Ok(())
}

async fn clear_uart_mappings(state: &AppState) -> crate::error::AppResult<()> {
    let mappings: UartMappings = HashMap::new();
    let contents = crate::store::encode_uart(&mappings)?;
    crate::store::atomic_replace(&control::paths().uart, contents.as_bytes())?;
    *state.uart_map.write().await = mappings;
    Ok(())
}

fn schedule_restart() {
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(750)).await;
        match tokio::process::Command::new("systemctl")
            .args(["restart", "dev-con.service"])
            .status()
            .await
        {
            Ok(status) if status.success() => {
                tracing::info!("EdgeController service restart requested successfully");
            }
            Ok(status) => {
                tracing::error!(%status, "systemctl rejected EdgeController restart");
            }
            Err(error) => {
                tracing::error!(%error, "cannot execute EdgeController service restart");
            }
        }
    });
}
