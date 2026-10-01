//! Gen5 USB mapping and CPLD power HTTP adapters.

use axum::{
    extract::{Extension, State},
    response::Response,
    Json,
};
use std::sync::Arc;

use crate::{jobs::Lease, state::AppState};

/// Return the stored UART and power interfaces for a Gen5 board.
pub async fn gen5_tty_entry_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<super::Gen5TtyRequest>,
) -> impl axum::response::IntoResponse {
    super::gen5_tty_entry(State(state), Json(request)).await
}

/// Set the mapped Gen5 CPLD power state.
pub async fn gen5_power_handler(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<super::Gen5PowerRequest>,
) -> Response {
    super::gen5_power(State(state), Extension(lease), Json(request)).await
}

/// Discover the connected Gen5 UART and power interfaces.
pub async fn mapping_entry_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<super::MappingEntryRequest>,
) -> Response {
    super::mapping_entry(State(state), Json(request)).await
}
