use axum::{
    extract::{Extension, State},
    Json,
    response::Response,
};
use std::sync::Arc;

use crate::{jobs::Lease, state::AppState};

pub async fn relay_handler(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<super::RelayRequest>,
) -> Response {
    super::relay(State(state), Extension(lease), Json(request)).await
}

pub async fn relay_status_handler(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<super::RelayStatusRequest>,
) -> Response {
    super::relay_status(State(state), Extension(lease), Json(request)).await
}

pub async fn relay_config_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<super::RelayConfigRequest>,
) -> Response {
    super::relay_config(State(state), Json(request)).await
}

pub async fn relay_delete_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<super::DeleteRequest>,
) -> Response {
    super::relay_delete(State(state), Json(request)).await
}

pub async fn devcon_delete_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<super::DeleteRequest>,
) -> Response {
    super::devcon_delete(State(state), Json(request)).await
}
