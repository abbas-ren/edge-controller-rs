//! Observability and external-facing service metadata.
//!
//! This module keeps the metrics collector and API documentation helpers
//! together so monitoring and docs can evolve independently from the device
//! control logic.

pub mod metrics;
pub mod swagger;
