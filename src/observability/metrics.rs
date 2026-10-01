//! Linux system metrics for the legacy heartbeat schema.
//!
//! The original sampler is kept for WebSocket heartbeat reporting. The module
//! also exposes a Prometheus registry for Grafana-friendly endpoint monitoring.

use crate::{
    error::{AppError, AppResult},
    models::HeartbeatPayload,
};

use axum::{response::IntoResponse, routing::get, Json, Router};
use prometheus::{
    Encoder, Gauge, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    Registry, TextEncoder,
};
use std::{
    fs,
    net::SocketAddr,
    path::Path,
    sync::{Arc, OnceLock},
    time::Instant,
};
use sysinfo::Disks;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
pub const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

#[derive(Default)]
pub struct MetricSampler {
    previous_cpu: Option<(u64, u64)>,
    previous_network: Option<(Instant, u64, u64)>,
}

#[derive(Clone)]
pub struct MetricsCollector {
    registry: Arc<Registry>,
    http_requests_total: IntCounterVec,
    http_request_duration_seconds: HistogramVec,
    firmware_flash_total: IntCounterVec,
    firmware_flash_duration_seconds: HistogramVec,
    registration_attempts_total: IntCounterVec,
    hardware_operations_total: IntCounterVec,
    hardware_operation_active: IntGauge,
    mapping_entries: IntGaugeVec,
    active_captures: IntGauge,
    websocket_connected: IntGauge,
    websocket_events_total: IntCounterVec,
}

impl MetricsCollector {
    pub fn new() -> Self {
        let registry = Arc::new(Registry::new());
        let http_requests_total = IntCounterVec::new(
            Opts::new(
                "edgecontroller_http_requests_total",
                "Total number of HTTP requests handled by the controller",
            ),
            &["method", "route", "status"],
        )
        .expect("HTTP request counter must be valid");
        let http_request_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "edgecontroller_http_request_duration_seconds",
                "Duration of HTTP requests in seconds",
            ),
            &["method", "route"],
        )
        .expect("Request duration histogram must be valid");
        let service_up = Gauge::new(
            "edgecontroller_up",
            "Indicates whether the controller is serving traffic",
        )
        .expect("service status gauge must be valid");
        let firmware_flash_total = IntCounterVec::new(
            Opts::new(
                "edgecontroller_firmware_flash_total",
                "Completed firmware flash operations by generation and outcome",
            ),
            &["generation", "outcome"],
        )
        .expect("Firmware flash counter must be valid");
        let firmware_flash_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "edgecontroller_firmware_flash_duration_seconds",
                "Firmware flash duration in seconds by generation",
            ),
            &["generation"],
        )
        .expect("Firmware flash duration histogram must be valid");
        let websocket_connected = IntGauge::new(
            "edgecontroller_websocket_connected",
            "Whether the backend WebSocket session is currently connected",
        )
        .expect("WebSocket connection gauge must be valid");
        let websocket_events_total = IntCounterVec::new(
            Opts::new(
                "edgecontroller_websocket_events_total",
                "Backend WebSocket lifecycle and frame events by bounded event type",
            ),
            &["event"],
        )
        .expect("WebSocket event counter must be valid");
        let registration_attempts_total = IntCounterVec::new(
            Opts::new(
                "edgecontroller_registration_attempts_total",
                "Backend registration attempts by outcome",
            ),
            &["outcome"],
        )
        .expect("Registration attempt counter must be valid");
        let hardware_operations_total = IntCounterVec::new(
            Opts::new(
                "edgecontroller_hardware_operations_total",
                "Hardware API operations by bounded operation name and outcome",
            ),
            &["operation", "outcome"],
        )
        .expect("Hardware operation counter must be valid");
        let hardware_operation_active = IntGauge::new(
            "edgecontroller_hardware_operation_active",
            "Whether one serialized hardware operation currently owns the lease",
        )
        .expect("Hardware operation gauge must be valid");
        let mapping_entries = IntGaugeVec::new(
            Opts::new(
                "edgecontroller_mapping_entries",
                "Persisted board mappings by mapping type",
            ),
            &["type"],
        )
        .expect("Mapping count gauge must be valid");
        let active_captures = IntGauge::new(
            "edgecontroller_active_captures",
            "RTOS capture sessions retained by the controller",
        )
        .expect("Active capture gauge must be valid");

        registry
            .register(Box::new(http_requests_total.clone()))
            .expect("HTTP request counter must be registered");
        registry
            .register(Box::new(http_request_duration_seconds.clone()))
            .expect("HTTP request duration histogram must be registered");
        registry
            .register(Box::new(service_up.clone()))
            .expect("service status gauge must be registered");
        registry
            .register(Box::new(firmware_flash_total.clone()))
            .expect("Firmware flash counter must be registered");
        registry
            .register(Box::new(firmware_flash_duration_seconds.clone()))
            .expect("Firmware flash duration histogram must be registered");
        registry
            .register(Box::new(websocket_connected.clone()))
            .expect("WebSocket connection gauge must be registered");
        registry
            .register(Box::new(websocket_events_total.clone()))
            .expect("WebSocket event counter must be registered");
        registry
            .register(Box::new(registration_attempts_total.clone()))
            .expect("Registration attempt counter must be registered");
        registry
            .register(Box::new(hardware_operations_total.clone()))
            .expect("Hardware operation counter must be registered");
        registry
            .register(Box::new(hardware_operation_active.clone()))
            .expect("Hardware operation gauge must be registered");
        registry
            .register(Box::new(mapping_entries.clone()))
            .expect("Mapping count gauge must be registered");
        registry
            .register(Box::new(active_captures.clone()))
            .expect("Active capture gauge must be registered");

        service_up.set(1.0);

        Self {
            registry,
            http_requests_total,
            http_request_duration_seconds,
            firmware_flash_total,
            firmware_flash_duration_seconds,
            registration_attempts_total,
            hardware_operations_total,
            hardware_operation_active,
            mapping_entries,
            active_captures,
            websocket_connected,
            websocket_events_total,
        }
    }

    pub fn record_firmware_flash(&self, generation: u8, success: bool, duration_seconds: f64) {
        let generation = generation.to_string();
        let outcome = if success { "success" } else { "failure" };
        self.firmware_flash_total
            .with_label_values(&[&generation, outcome])
            .inc();
        self.firmware_flash_duration_seconds
            .with_label_values(&[&generation])
            .observe(duration_seconds);
    }

    pub fn set_websocket_connected(&self, connected: bool) {
        self.websocket_connected.set(i64::from(connected));
    }

    pub fn record_websocket_event(&self, event: &'static str) {
        debug_assert!(matches!(
            event,
            "connected"
                | "reconnect_clean"
                | "reconnect_error"
                | "heartbeat_sent"
                | "text_received"
                | "binary_received"
                | "ping_received"
                | "pong_received"
        ));
        self.websocket_events_total
            .with_label_values(&[event])
            .inc();
    }

    pub fn record_registration_attempt(&self, outcome: &'static str) {
        debug_assert!(matches!(
            outcome,
            "accepted" | "rejected" | "transport_error"
        ));
        self.registration_attempts_total
            .with_label_values(&[outcome])
            .inc();
    }

    pub fn record_hardware_operation(&self, operation: &str, success: bool) {
        let outcome = if success { "success" } else { "failure" };
        self.hardware_operations_total
            .with_label_values(&[operation, outcome])
            .inc();
    }

    pub fn set_hardware_operation_active(&self, active: bool) {
        self.hardware_operation_active.set(i64::from(active));
    }

    pub fn set_mapping_counts(&self, usb: usize, gen5: usize) {
        self.mapping_entries
            .with_label_values(&["usb"])
            .set(saturating_i64(usb));
        self.mapping_entries
            .with_label_values(&["gen5"])
            .set(saturating_i64(gen5));
    }

    pub fn set_active_captures(&self, count: usize) {
        self.active_captures.set(saturating_i64(count));
    }

    pub fn record_http_request(
        &self,
        method: &str,
        route: &str,
        status: u16,
        duration_seconds: f64,
    ) {
        self.http_requests_total
            .with_label_values(&[method, route, &status.to_string()])
            .inc();
        self.http_request_duration_seconds
            .with_label_values(&[method, route])
            .observe(duration_seconds);
    }

    pub fn render(&self) -> String {
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        let encoder = TextEncoder::new();
        encoder
            .encode(&metric_families, &mut buffer)
            .expect("Prometheus encoder must succeed");
        String::from_utf8(buffer).expect("Prometheus output must be UTF-8")
    }
}

fn saturating_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub fn global_metrics() -> &'static MetricsCollector {
    static GLOBAL: OnceLock<MetricsCollector> = OnceLock::new();
    GLOBAL.get_or_init(MetricsCollector::new)
}

async fn metrics_handler() -> impl IntoResponse {
    let output = global_metrics().render();
    (
        axum::http::StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)],
        output,
    )
}

async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "service": "edgecontroller" }))
}

pub async fn serve_metrics(bind_address: SocketAddr) {
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/health", get(health_handler));

    let listener = match tokio::net::TcpListener::bind(bind_address).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!(%bind_address, %error, "metrics exporter bind failed");
            return;
        }
    };

    tracing::info!(address = %listener.local_addr().unwrap_or(bind_address), "metrics exporter listening on port 8081");
    let _ = axum::serve(listener, app).await;
}

fn percentage(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

fn parse_counter(value: Option<&str>, name: &str) -> AppResult<u64> {
    value
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| AppError::Msg(format!("invalid Linux metric: {name}")))
}

/// Return (idle including iowait, total CPU ticks).
///
/// Do not add guest/guest_nice: Linux already includes those in user/nice.
fn cpu_ticks() -> AppResult<(u64, u64)> {
    let text = fs::read_to_string("/proc/stat")?;
    let line = text
        .lines()
        .find(|line| line.starts_with("cpu "))
        .ok_or_else(|| AppError::Msg("aggregate CPU counters missing".into()))?;

    let values = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| AppError::Msg("invalid CPU tick counter".into()))
        })
        .collect::<AppResult<Vec<_>>>()?;

    if values.len() < 4 {
        return Err(AppError::Msg("incomplete CPU tick counters".into()));
    }

    let idle = values[3].saturating_add(values.get(4).copied().unwrap_or(0));
    let total = values
        .iter()
        .fold(0_u64, |total, value| total.saturating_add(*value));

    Ok((idle, total))
}

fn frequency_from_sysfs(path: &str) -> Option<f64> {
    let khz = fs::read_to_string(path).ok()?.trim().parse::<f64>().ok()?;

    (khz.is_finite() && khz > 0.0).then_some(khz / 1_000_000.0)
}

fn cpu_frequencies() -> (f64, f64) {
    let current = frequency_from_sysfs("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq")
        .or_else(|| {
            let cpuinfo = fs::read_to_string("/proc/cpuinfo").ok()?;

            cpuinfo.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;

                if key.trim() != "cpu MHz" {
                    return None;
                }

                let mhz = value.trim().parse::<f64>().ok()?;
                (mhz.is_finite() && mhz > 0.0).then_some(mhz / 1000.0)
            })
        })
        .unwrap_or(0.0);

    let maximum = frequency_from_sysfs("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq")
        .unwrap_or(current);

    (current, maximum)
}

fn memory_bytes() -> AppResult<(u64, u64)> {
    let text = fs::read_to_string("/proc/meminfo")?;
    let mut total = None;
    let mut available = None;

    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };

        if matches!(key, "MemTotal" | "MemAvailable") {
            let kib = parse_counter(value.split_whitespace().next(), key)?;
            let bytes = kib.saturating_mul(1024);

            match key {
                "MemTotal" => total = Some(bytes),
                "MemAvailable" => available = Some(bytes),
                _ => {}
            }
        }
    }

    let total = total
        .filter(|total| *total > 0)
        .ok_or_else(|| AppError::Msg("MemTotal missing or zero".into()))?;

    let available = available.ok_or_else(|| AppError::Msg("MemAvailable missing".into()))?;

    Ok((total.saturating_sub(available), total))
}

fn network_bytes() -> AppResult<(u64, u64)> {
    let text = fs::read_to_string("/proc/net/dev")?;
    let mut received = 0_u64;
    let mut transmitted = 0_u64;

    for line in text.lines().skip(2) {
        let Some((interface, counters)) = line.split_once(':') else {
            continue;
        };

        let interface = interface.trim();
        if !(interface.starts_with("eth") || interface.starts_with("en")) {
            continue;
        }

        let mut fields = counters.split_whitespace();

        let rx = parse_counter(fields.next(), "network RX bytes")?;
        let tx = parse_counter(fields.nth(7), "network TX bytes")?;

        received = received.saturating_add(rx);
        transmitted = transmitted.saturating_add(tx);
    }

    Ok((received, transmitted))
}

fn root_disk_bytes() -> AppResult<(u64, u64)> {
    let disks = Disks::new_with_refreshed_list();

    let root = disks
        .list()
        .iter()
        .find(|disk| disk.mount_point() == Path::new("/"))
        .ok_or_else(|| AppError::Msg("root filesystem statistics unavailable".into()))?;

    let total = root.total_space();
    if total == 0 {
        return Err(AppError::Msg(
            "root filesystem reports zero capacity".into(),
        ));
    }

    Ok((total.saturating_sub(root.available_space()), total))
}

impl MetricSampler {
    pub fn heartbeat(&mut self, uid: String, ip: String) -> AppResult<HeartbeatPayload> {
        let (idle, total_ticks) = cpu_ticks()?;
        let (received, transmitted) = network_bytes()?;
        let (memory_used, memory_total) = memory_bytes()?;
        let (disk_used, disk_total) = root_disk_bytes()?;
        let (cpu_current, cpu_maximum) = cpu_frequencies();

        let now = Instant::now();

        let cpu_usage = match self.previous_cpu {
            Some((previous_idle, previous_total))
                if total_ticks >= previous_total && idle >= previous_idle =>
            {
                let total_delta = total_ticks - previous_total;
                let idle_delta = idle - previous_idle;
                percentage(total_delta.saturating_sub(idle_delta), total_delta)
            }
            _ => 0.0,
        };

        let (upload, download) = match self.previous_network {
            Some((previous_time, previous_rx, previous_tx)) => {
                let elapsed = now.duration_since(previous_time).as_secs_f64();

                if elapsed > 0.0 {
                    (
                        transmitted.saturating_sub(previous_tx) as f64 * 8.0
                            / elapsed
                            / 1_000_000.0,
                        received.saturating_sub(previous_rx) as f64 * 8.0 / elapsed / 1_000_000.0,
                    )
                } else {
                    (0.0, 0.0)
                }
            }
            None => (0.0, 0.0),
        };

        self.previous_cpu = Some((idle, total_ticks));
        self.previous_network = Some((now, received, transmitted));

        Ok(HeartbeatPayload {
            msg_type: "heartbeat".into(),
            uid,
            ip,
            timestamp: chrono::Utc::now().timestamp(),
            cpu_current: format!("{cpu_current:.1} GHz"),
            cpu_total: format!("{cpu_maximum:.1} GHz"),
            cpu_usage_percent: cpu_usage,
            memory_used: format!("{:.1} GB", memory_used as f64 / GIB),
            memory_total: format!("{:.1} GB", memory_total as f64 / GIB),
            memory_usage_percent: percentage(memory_used, memory_total),
            network_upload: format!("{upload:.1} Mbps"),
            network_download: format!("{download:.1} Mbps"),
            disk_used: format!("{:.1} GB", disk_used as f64 / GIB),
            disk_total: format!("{:.1} GB", disk_total as f64 / GIB),
            disk_usage_percent: percentage(disk_used, disk_total),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{percentage, MetricsCollector};

    #[test]
    fn percentage_handles_zero_capacity() {
        assert_eq!(percentage(0, 0), 0.0);
    }

    #[test]
    fn percentage_is_bounded() {
        assert_eq!(percentage(25, 100), 25.0);
        assert_eq!(percentage(150, 100), 100.0);
    }

    #[test]
    fn http_metrics_record_status_and_elapsed_time() {
        let metrics = MetricsCollector::new();
        metrics.record_http_request("GET", "/health", 200, 0.025);

        let output = metrics.render();
        assert!(output.contains(
            "edgecontroller_http_requests_total{method=\"GET\",route=\"/health\",status=\"200\"} 1"
        ));
        assert!(output.contains(
            "edgecontroller_http_request_duration_seconds_sum{method=\"GET\",route=\"/health\"} 0.025"
        ));
    }

    #[test]
    fn operation_metrics_record_flash_and_websocket_state() {
        let metrics = MetricsCollector::new();
        metrics.record_firmware_flash(3, true, 12.5);
        metrics.set_websocket_connected(true);
        metrics.record_websocket_event("connected");
        metrics.record_registration_attempt("rejected");
        metrics.record_hardware_operation("relay", false);
        metrics.set_hardware_operation_active(true);
        metrics.set_mapping_counts(2, 3);
        metrics.set_active_captures(4);

        let output = metrics.render();
        assert!(output.contains(
            "edgecontroller_firmware_flash_total{generation=\"3\",outcome=\"success\"} 1"
        ));
        assert!(output
            .contains("edgecontroller_firmware_flash_duration_seconds_sum{generation=\"3\"} 12.5"));
        assert!(output.contains("edgecontroller_websocket_connected 1"));
        assert!(output.contains("edgecontroller_websocket_events_total{event=\"connected\"} 1"));
        assert!(
            output.contains("edgecontroller_registration_attempts_total{outcome=\"rejected\"} 1")
        );
        assert!(output.contains(
            "edgecontroller_hardware_operations_total{operation=\"relay\",outcome=\"failure\"} 1"
        ));
        assert!(output.contains("edgecontroller_hardware_operation_active 1"));
        assert!(output.contains("edgecontroller_mapping_entries{type=\"usb\"} 2"));
        assert!(output.contains("edgecontroller_mapping_entries{type=\"gen5\"} 3"));
        assert!(output.contains("edgecontroller_active_captures 4"));

        metrics.set_websocket_connected(false);
        assert!(metrics
            .render()
            .contains("edgecontroller_websocket_connected 0"));
    }
}
