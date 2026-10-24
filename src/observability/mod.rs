//! Observability and external-facing service metadata.
//!
//! This module keeps the metrics collector and API documentation helpers
//! together so monitoring and docs can evolve independently from the device
//! control logic.

pub mod metrics;
pub mod swagger;

/// RAII guard that emits structured enter/exit tracing for API call-flow functions.
///
/// Farm-controller proxies UI admin/control/hardware operations onto these
/// endpoints, so every handler and nested helper in that path should install a
/// guard at entry. Exit is logged when the guard drops (including early returns).
pub struct ApiTrace {
    module: &'static str,
    fn_name: &'static str,
}

impl ApiTrace {
    /// Log function entry and return a guard that logs exit on drop.
    pub fn enter(module: &'static str, fn_name: &'static str) -> Self {
        tracing::info!(
            target: "api_flow",
            module,
            fn_name,
            "enter"
        );
        Self { module, fn_name }
    }
}

impl Drop for ApiTrace {
    fn drop(&mut self) {
        if std::thread::panicking() {
            tracing::error!(
                target: "api_flow",
                module = self.module,
                fn_name = self.fn_name,
                "exit panic"
            );
        } else {
            tracing::info!(
                target: "api_flow",
                module = self.module,
                fn_name = self.fn_name,
                "exit"
            );
        }
    }
}

/// Log an API-flow failure at warn (4xx) or error (5xx).
pub fn api_fail(fn_name: &'static str, status: u16, message: &str) {
    if status >= 500 {
        tracing::error!(
            target: "api_flow",
            fn_name,
            status,
            message,
            "api failure"
        );
    } else {
        tracing::warn!(
            target: "api_flow",
            fn_name,
            status,
            message,
            "api failure"
        );
    }
}
