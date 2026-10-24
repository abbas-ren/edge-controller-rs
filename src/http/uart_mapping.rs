//! Generation-aware UART mapping HTTP adapter.

use axum::{
    extract::{Extension, State},
    response::Response,
    Json,
};
use std::sync::Arc;

use crate::{jobs::Lease, state::AppState};

pub async fn uart_config_handler(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<super::UartConfigRequest>,
) -> Response {
    let _api_trace = crate::observability::ApiTrace::enter("http::uart_mapping", "uart_config_handler");
    super::uart_config(State(state), Extension(lease), Json(request)).await
}
