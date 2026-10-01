use axum::{
    extract::State,
    Json,
    response::Response,
};
use std::sync::Arc;

use crate::state::AppState;

pub async fn rtos_start_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<super::RtosStartRequest>,
) -> Response {
    super::rtos_start(State(state), Json(request)).await
}

pub async fn rtos_end_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<super::RtosEndRequest>,
) -> Response {
    super::rtos_end(State(state), Json(request)).await
}
