//! Gen3/Gen4 relay and controller-mapping HTTP adapters.

use axum::{
    extract::{Extension, State},
    response::Response,
    Json,
};
use std::sync::Arc;

use crate::{jobs::Lease, state::AppState};

/// Set one approved relay channel on or off.
pub async fn relay_handler(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<super::RelayRequest>,
) -> Response {
    super::relay(State(state), Extension(lease), Json(request)).await
}

/// Read one approved relay channel's state.
pub async fn relay_status_handler(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<super::RelayStatusRequest>,
) -> Response {
    super::relay_status(State(state), Extension(lease), Json(request)).await
}

/// Validate and persist a board-to-relay mapping.
pub async fn relay_config_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<super::RelayConfigRequest>,
) -> Response {
    super::relay_config(State(state), Json(request)).await
}

/// Delete matching relay mappings without stopping the service.
pub async fn relay_delete_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<super::DeleteRequest>,
) -> Response {
    super::relay_delete(State(state), Json(request)).await
}

/// Delete controller mappings and optionally request service shutdown.
pub async fn devcon_delete_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<super::DeleteRequest>,
) -> Response {
    super::devcon_delete(State(state), Json(request)).await
}
