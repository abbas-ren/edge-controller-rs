use axum::{
    extract::{Extension, State},
    Json,
    response::Response,
};
use std::sync::Arc;

use crate::{jobs::Lease, state::AppState};

pub async fn ipl_run_handler(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<super::IplRequest>,
) -> Response {
    super::ipl_run(State(state), Extension(lease), Json(request)).await
}

pub async fn ipl_mode_handler(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<super::IplModeRequest>,
) -> Response {
    super::ipl_mode(State(state), Extension(lease), Json(request)).await
}

pub async fn ipl_mode_default_handler(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<super::IplModeRequest>,
) -> Response {
    super::ipl_mode_default(State(state), Extension(lease), Json(request)).await
}

pub async fn reboot_device_handler(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<super::RebootDeviceRequest>,
) -> Response {
    super::reboot_device(State(state), Extension(lease), Json(request)).await
}

pub async fn ipl_remove_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<super::RemoveIplRequest>,
) -> Response {
    super::ipl_remove(State(state), Json(request)).await
}
