//! RTOS serial capture lifecycle HTTP adapters.

use axum::{extract::State, response::Response, Json};
use std::sync::Arc;

use crate::state::AppState;

/// Start a bounded capture for a mapped board console.
pub async fn rtos_start_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<super::RtosStartRequest>,
) -> Response {
    super::rtos_start(State(state), Json(request)).await
}

/// Stop a capture and stream its collected bytes to the client.
pub async fn rtos_end_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<super::RtosEndRequest>,
) -> Response {
    super::rtos_end(State(state), Json(request)).await
}
