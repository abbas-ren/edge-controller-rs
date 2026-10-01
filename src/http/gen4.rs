//! Gen3/Gen4 FlashWriter and shared device-control HTTP adapters.

use axum::{
    extract::{Extension, State},
    response::Response,
    Json,
};
use std::sync::Arc;

use crate::{jobs::Lease, state::AppState};

/// Accept a generation-specific firmware flash request.
pub async fn ipl_run_handler(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<super::IplRequest>,
) -> Response {
    super::ipl_run(State(state), Extension(lease), Json(request)).await
}

/// Put the configured Gen3/Gen4 board into download mode.
pub async fn ipl_mode_handler(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<super::IplModeRequest>,
) -> Response {
    super::ipl_mode(State(state), Extension(lease), Json(request)).await
}

/// Restore the configured Gen3/Gen4 board's default boot mode.
pub async fn ipl_mode_default_handler(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<super::IplModeRequest>,
) -> Response {
    super::ipl_mode_default(State(state), Extension(lease), Json(request)).await
}

/// Power-cycle a mapped Gen5 board through its CPLD power interface.
pub async fn reboot_device_handler(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<super::RebootDeviceRequest>,
) -> Response {
    super::reboot_device(State(state), Extension(lease), Json(request)).await
}

/// Remove a validated firmware package directory.
pub async fn ipl_remove_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<super::RemoveIplRequest>,
) -> Response {
    super::ipl_remove(State(state), Json(request)).await
}
