mod gen4;
mod gen5;
mod relay;
mod rtos;

use crate::{jobs::Lease, state::RtosSession};
use axum::{
    body::Body,
    extract::{Extension, MatchedPath, State},
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};

use std::{
    fs,
    io::Write,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::task;
use tracing::{debug, error, info, warn};

use crate::store::{Gen5Mappings, UsbMappings};
use crate::{
    config::*,
    ipl,
    models::*,
    observability::metrics::global_metrics,
    state::AppState,
    uart::{open_uart, write_to_path},
    usb::*,
};

const FIRMWARE_ROOT: &str = "/var/lib/dev-controller/firmware";

const CAPTURE_ROOT: &str = "/var/lib/dev-controller/captures";
const MAX_CAPTURE_SESSIONS: usize = 16;
const MAX_CAPTURE_BYTES: u64 = 64 * 1024 * 1024;
/// Serialize mapping discovery and mapping-file mutations.
///
/// Discovery deliberately changes board power, so two discovery operations
/// must not run concurrently. This mutex is acquired only from blocking
/// workers, never directly from an async handler.
///
/// This does not serialize the separate relay, power, or IPL endpoints.
/// Clients must not issue those operations while discovery is running.
static MAPPING_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Compare equal-length tokens without early exit on differing bytes.
///
/// Token length is not secret. This avoids an obvious byte-by-byte timing
/// leak without introducing an additional authentication dependency.
fn token_matches(expected: &[u8], supplied: &[u8]) -> bool {
    if expected.len() != supplied.len() {
        debug!(
            expected_len = expected.len(),
            supplied_len = supplied.len(),
            "API token length mismatch"
        );
        return false;
    }

    let matches = expected
        .iter()
        .zip(supplied)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0;

    if !matches {
        debug!("API token bytes did not match; rejecting request");
    }

    matches
}

fn is_hardware_operation(path: &str) -> bool {
    matches!(
        path,
        "/relay"
            | "/relay/status"
            | "/relay/identity"
            | "/relay/config"
            | "/relay/delete"
            | "/devCon/delete"
            | "/ipl"
            | "/ipl-mode"
            | "/ipl-mode/default"
            | "/gen5/tty_entry"
            | "/gen5/power"
            | "/mapping/entry"
            | "/reboot-device"
            | "/ipl/remove"
            | "/rtos/start"
            | "/rtos/end"
    )
}

async fn request_guard(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let metric_route = request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
        .unwrap_or("unmatched")
        .to_owned();
    let started = std::time::Instant::now();
    let response = guarded_request(state, request, next).await;

    global_metrics().record_http_request(
        method.as_str(),
        metric_route.as_str(),
        response.status().as_u16(),
        started.elapsed().as_secs_f64(),
    );
    if is_hardware_operation(&metric_route) {
        global_metrics().record_hardware_operation(
            metric_route.trim_start_matches('/'),
            response.status().is_success(),
        );
    }

    response
}

async fn guarded_request(state: Arc<AppState>, mut request: Request<Body>, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let content_length = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());

    info!(
        method = %method,
        path = %path,
        content_type = ?content_type,
        content_length = ?content_length,
        "router API request received"
    );

    debug!(path = %request.uri(), "HTTP request entering request guard");

    if let Some(expected) = state.api_token.as_deref() {
        let supplied = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or("");

        if !token_matches(expected.as_bytes(), supplied.as_bytes()) {
            return error_response("missing or invalid bearer token", StatusCode::UNAUTHORIZED);
        }
    }

    let relay_feature_disabled = matches!(
        path.as_str(),
        "/relay"
            | "/relay/status"
            | "/relay/identity"
            | "/relay/config"
            | "/relay/delete"
            | "/devCon/delete"
            | "/ipl-mode"
            | "/ipl-mode/default"
    ) && !(state.features.gen3 || state.features.gen4);
    let gen5_feature_disabled = matches!(
        path.as_str(),
        "/gen5/tty_entry" | "/gen5/power" | "/mapping/entry" | "/reboot-device"
    ) && !state.features.gen5;
    let ipl_feature_disabled = matches!(path.as_str(), "/ipl" | "/ipl/remove")
        && !(state.features.gen3 || state.features.gen4 || state.features.gen5);
    if (path.as_str() == "/rtos/start" || path.as_str() == "/rtos/end") && !state.features.rtos {
        return error_response(
            "RTOS capture is disabled; start the service with --enable-rtos",
            StatusCode::NOT_IMPLEMENTED,
        );
    }
    if relay_feature_disabled {
        return error_response(
            "Gen3/Gen4 functionality is disabled; start the service with --enable-gen3 or --enable-gen4",
            StatusCode::NOT_IMPLEMENTED,
        );
    }
    if gen5_feature_disabled {
        return error_response(
            "Gen5 functionality is disabled; start the service with --enable-gen5",
            StatusCode::NOT_IMPLEMENTED,
        );
    }
    if ipl_feature_disabled {
        return error_response(
            "IPL functionality is disabled; enable Gen3, Gen4, or Gen5",
            StatusCode::NOT_IMPLEMENTED,
        );
    }

    if state.jobs.is_closing() {
        return error_response(
            "controller is shutting down",
            StatusCode::SERVICE_UNAVAILABLE,
        );
    }

    let path = request.uri().path().to_owned();

    let hardware_operation = is_hardware_operation(path.as_str());

    if !hardware_operation {
        tracing::debug!(path = %request.uri(), "non-hardware request passed through without a lease");
        return next.run(request).await;
    }

    tracing::info!(path = %request.uri(), "accepting hardware operation under lease");

    let lease = match state.jobs.try_enter() {
        Ok(lease) => lease,
        Err(message) => {
            return error_response(message, StatusCode::CONFLICT);
        }
    };

    // Starting/stopping captures is serialized, but multiple established
    // capture sessions are allowed.
    let capture_lifecycle = matches!(path.as_str(), "/rtos/start" | "/rtos/end");

    if !capture_lifecycle {
        let active_capture_count = state.rtos_sessions.lock().await.len();
        if active_capture_count > 0 {
            tracing::warn!(
                active_capture_count,
                path = %request.uri(),
                "hardware request blocked while captures remain active"
            );
            return error_response(
                "stop and download RTOS captures before changing hardware",
                StatusCode::CONFLICT,
            );
        }
    }

    // IPL handlers transfer a clone into their tracked background job.
    request.extensions_mut().insert(lease.clone());

    let (sender, receiver) = tokio::sync::oneshot::channel();

    // Track the entire accepted request, not just IPL background work.
    // If its client disconnects, the handler still runs to completion and
    // retains the hardware lease.
    state.jobs.spawn(lease, async move {
        let response = next.run(request).await;

        // A disconnected client is not a hardware-operation failure.
        let _ = sender.send(response);
    });

    match receiver.await {
        Ok(response) => response,
        Err(_) => error_response(
            "hardware request worker terminated unexpectedly",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

fn mapping_guard() -> crate::error::AppResult<std::sync::MutexGuard<'static, ()>> {
    MAPPING_LOCK.lock().map_err(|error| {
        tracing::error!(%error, "mapping operation mutex was poisoned");
        crate::error::AppError::Msg("mapping operation mutex was poisoned".into())
    })
}

/// Ensure a value can safely occupy one field in the legacy CSV format.
///
/// We deliberately reject CSV delimiters instead of introducing quoting
/// that the original project's CSV readers would not understand.
fn validate_csv_field(name: &str, value: &str) -> crate::error::AppResult<()> {
    if value.is_empty()
        || value.len() > 255
        || value.contains(',')
        || value.chars().any(char::is_control)
    {
        warn!(
            field = %name,
            value_len = value.len(),
            contains_comma = value.contains(','),
            has_control_chars = value.chars().any(char::is_control),
            "rejecting invalid CSV field value"
        );
        return Err(crate::error::AppError::Msg(format!(
            "{name} must be nonempty, at most 255 bytes, \
             and contain no commas or control characters"
        )));
    }

    debug!(field = %name, value_len = value.len(), "CSV field validated");
    Ok(())
}

/// Call only from blocking workers while holding MAPPING_LOCK.
fn persist_usb_mappings(state: &AppState, mappings: &UsbMappings) -> crate::error::AppResult<()> {
    let other = state.gen5_map.blocking_read();

    tracing::debug!(
        usb_count = mappings.len(),
        gen5_count = other.len(),
        "persisting USB mappings"
    );
    crate::store::validate_snapshot(mappings, &other, &state.hardware)?;

    let contents = crate::store::encode_usb(mappings)?;
    crate::store::atomic_replace(Path::new(USB_MAPPING_FILE), contents.as_bytes())?;
    global_metrics().set_mapping_counts(mappings.len(), other.len());

    tracing::info!(usb_count = mappings.len(), path = %USB_MAPPING_FILE, "USB mapping state persisted");
    Ok(())
}

/// Call only from blocking workers while holding MAPPING_LOCK.
fn persist_gen5_mappings(state: &AppState, mappings: &Gen5Mappings) -> crate::error::AppResult<()> {
    let other = state.usb_map.blocking_read();

    tracing::debug!(
        gen5_count = mappings.len(),
        usb_count = other.len(),
        "persisting Gen5 mappings"
    );
    crate::store::validate_snapshot(&other, mappings, &state.hardware)?;

    let contents = crate::store::encode_gen5(mappings)?;
    crate::store::atomic_replace(Path::new(GEN5_MAPPING_FILE), contents.as_bytes())?;
    global_metrics().set_mapping_counts(other.len(), mappings.len());

    tracing::info!(gen5_count = mappings.len(), path = %GEN5_MAPPING_FILE, "Gen5 mapping state persisted");
    Ok(())
}

/// Observe a boot after cycling the selected power source.
///
/// The UART must already be open before power-on, otherwise the early
/// boot prompt can be missed. Clear stale UART input while power is off.
///
/// After probing, cycle power again to release the board from the U-Boot
/// prompt. This follows the legacy discovery behavior: leave power ON,
/// rather than restoring the original power state.
///
/// Cleanup errors are propagated; a failed reset is not reported as success.
fn probe_with_power<F>(
    mut port: Box<dyn serialport::SerialPort>,
    off_delay: Duration,
    mut set_power: F,
) -> crate::error::AppResult<Option<String>>
where
    F: FnMut(bool) -> crate::error::AppResult<()>,
{
    use crate::error::AppError;

    tracing::debug!(
        off_delay_ms = off_delay.as_millis(),
        "starting board power-cycle probe"
    );

    let observation = (|| {
        set_power(false)?;
        std::thread::sleep(off_delay);

        port.clear(serialport::ClearBuffer::Input)
            .map_err(|e| AppError::Msg(format!("could not clear stale UART input: {e}")))?;

        set_power(true)?;

        read_uboot_mac(&mut *port, Duration::from_secs(20))
    })();

    // Release the serial device before restarting normal boot.
    drop(port);

    let restart = (|| {
        set_power(false)?;
        std::thread::sleep(Duration::from_secs(1));
        set_power(true)
    })();

    match (observation, restart) {
        (Ok(found), Ok(())) => {
            tracing::info!(observed_mac = ?found, "board power-cycle probe completed successfully");
            Ok(found)
        }
        (Err(error), Ok(())) => {
            tracing::warn!(%error, "board probe failed after power cycle, but reset completed");
            Err(error)
        }
        (Ok(_), Err(error)) => {
            tracing::warn!(%error, "board probe succeeded but power reset failed");
            Err(error)
        }
        (Err(probe), Err(reset)) => {
            tracing::error!(probe = %probe, reset = %reset, "UART probe and board restart both failed");
            Err(AppError::Msg(format!(
                "UART probe failed: {probe}; board restart also failed: {reset}"
            )))
        }
    }
}

/// Discover a Gen3/Gen4 console using a specific relay channel.
///
/// Call only from spawn_blocking: blocking_lock/blocking_read and serial
/// operations must not execute on a Tokio async worker.
fn map_usb_to_mac_sync(
    state: Arc<AppState>,
    mac: String,
    serial: String,
    channel: u8,
    generation: Generation,
) -> crate::error::AppResult<bool> {
    use crate::error::AppError;

    let mac = validated_mac(&mac)?;
    validate_csv_field("relay serial", &serial)?;

    let baud = match generation {
        Generation::Gen3 => 115_200,
        Generation::Gen4 => 921_600,
        Generation::Gen5 => {
            return Err(AppError::Msg(
                "Gen5 requires a power-controller mapping".into(),
            ));
        }
    };

    let _operation = mapping_guard()?;

    tracing::info!(
        mac = %mac,
        serial = %serial,
        channel,
        generation = ?generation,
        "beginning verified UART mapping for board"
    );

    let binding = state.hardware.board(&mac, generation.as_int())?;
    let relay = binding
        .relay
        .as_ref()
        .ok_or_else(|| AppError::Msg("approved relay binding missing".into()))?;

    tracing::debug!(
        mac = %mac,
        approved_uart = ?binding.uart,
        approved_relay = ?relay,
        "approved board wiring loaded from hardware policy"
    );

    if state.relay.identity()?.serial_number != serial || relay.channel != channel {
        tracing::warn!(
            requested_serial = %serial,
            requested_channel = channel,
            approved_channel = relay.channel,
            "requested relay/channel differs from approved wiring"
        );
        return Err(AppError::Msg(
            "requested relay/channel differs from approved wiring".into(),
        ));
    }

    let tty = crate::usb::resolve_identity(&binding.uart)?;

    tracing::info!(
        mac = %mac,
        tty = %tty,
        uart_identity = ?binding.uart,
        "resolved approved UART to the live ttyUSB node"
    );

    // Do not accept an old ttyUSB pathname as a verified cache hit.
    let port = open_uart(&tty, baud)?;
    let observed = probe_with_power(port, Duration::from_secs(3), |on| {
        state.relay.set_channel(&serial, channel, on)
    })?;

    tracing::debug!(
        mac = %mac,
        tty = %tty,
        observed = ?observed,
        "board probe completed; checking whether the observed MAC matches the requested one"
    );

    if observed.as_deref() != Some(mac.as_str()) {
        tracing::warn!(
            mac = %mac,
            tty = %tty,
            observed = ?observed,
            "board probe did not match the requested MAC; mapping not persisted"
        );
        return Ok(false);
    }

    tracing::info!(%mac, %tty, "board UART mapping verified and will be persisted");

    let mut updated = state.usb_map.blocking_read().clone();

    // Remove stale entries for this board only after successful verification.
    updated.retain(|(stored_mac, _, _), _| stored_mac != &mac);

    if updated.values().any(|stored_tty| stored_tty == &tty) {
        return Err(AppError::Msg(
            "resolved UART is already stored for another board; \
             remove the stale mapping before retrying"
                .into(),
        ));
    }

    updated.insert((mac, serial, channel), tty);

    persist_usb_mappings(&state, &updated)?;
    *state.usb_map.blocking_write() = updated;

    Ok(true)
}

/// Probe one Gen5 UART/power pair.
///
/// The C implementation writes POWER#ON/OFF directly to the power TTY,
/// without configuring its baud rate. Preserve that behavior here rather
/// than guessing an undocumented power-controller baud rate.
///
/// Configure the power-controller serial settings during deployment if
/// its firmware requires settings different from the device defaults.
fn probe_gen5_pair(uart: &str, power: &str) -> crate::error::AppResult<Option<String>> {
    let uart = checked_tty(uart)?;
    let power = checked_tty(power)?;

    if uart == power {
        return Err(crate::error::AppError::Msg(
            "UART and power controller must be different devices".into(),
        ));
    }

    // serialport's Linux implementation supports custom baud rates.
    // Do not configure a second descriptor and then reopen at another baud.
    let port = open_uart(&uart, 1_843_200)?;

    probe_with_power(port, Duration::from_secs(1), |on| {
        write_to_path(&power, if on { "POWER#ON\n" } else { "POWER#OFF\n" })
    })
}

/// Verify an existing Gen5 mapping, otherwise discover a replacement.
///
/// Resources assigned to other MACs are excluded. An invalid old mapping
/// remains stored until a replacement is positively identified.
fn resolve_gen5_mapping_sync(
    state: Arc<AppState>,
    mac: &str,
) -> crate::error::AppResult<Option<(String, String)>> {
    use crate::error::AppError;

    tracing::debug!(target_mac = %mac, "starting Gen5 mapping resolution");
    let mac = validated_mac(mac)?;
    let _operation = mapping_guard()?;

    let binding = state.hardware.board(&mac, 5)?;
    let power_identity = binding
        .power
        .as_ref()
        .ok_or_else(|| AppError::Msg("approved power binding missing".into()))?;

    let uart = crate::usb::resolve_identity(&binding.uart)?;
    let power = crate::usb::resolve_identity(power_identity)?;

    if uart == power {
        return Err(AppError::Msg(
            "UART and power controller resolved to the same device".into(),
        ));
    }

    // Probe only the administrator-approved pair.
    // This verifies the UART MAC, not the electrical power wiring.
    let observed = probe_gen5_pair(&uart, &power)?;

    if observed.as_deref() != Some(mac.as_str()) {
        tracing::warn!(
            mac = %mac,
            uart = %uart,
            power = %power,
            observed = ?observed,
            "Gen5 mapping probe did not confirm the requested board MAC"
        );
        return Ok(None);
    }

    tracing::info!(%mac, %uart, %power, "Gen5 board mapping verified");

    let mut updated = state.gen5_map.blocking_read().clone();
    updated.remove(&mac);

    if updated.values().any(|entry| {
        entry.uart == uart || entry.power == power || entry.uart == power || entry.power == uart
    }) {
        return Err(AppError::Msg(
            "resolved device conflicts with another stored mapping; \
             remove stale mappings before retrying"
                .into(),
        ));
    }

    updated.insert(
        mac.clone(),
        Gen5MapEntry {
            uart: uart.clone(),
            power: power.clone(),
            mac,
        },
    );

    persist_gen5_mappings(&state, &updated)?;
    *state.gen5_map.blocking_write() = updated;

    Ok(Some((uart, power)))
}

async fn health_handler() -> Response {
    debug!("service health endpoint invoked");
    ok().into_response()
}

fn readiness_status(uid_present: bool, shutting_down: bool) -> StatusCode {
    if uid_present && !shutting_down {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn readiness_handler(State(state): State<Arc<AppState>>) -> Response {
    let registered = state.controller.read().await.uid.is_some();
    let shutting_down = state.jobs.is_closing();
    let status = readiness_status(registered, shutting_down);

    (
        status,
        Json(serde_json::json!({
            "ready": status == StatusCode::OK,
            "registered": registered,
            "shutting_down": shutting_down,
        })),
    )
        .into_response()
}

async fn status_handler(State(state): State<Arc<AppState>>) -> Response {
    let (board_mac, board_ip, uid) = {
        let controller = state.controller.read().await;
        (
            controller.board_mac.clone(),
            controller.board_ip.clone(),
            controller.uid.clone(),
        )
    };
    let usb_mapping_count = state.usb_map.read().await.len();
    let gen5_mapping_count = state.gen5_map.read().await.len();
    let active_capture_count = state.rtos_sessions.lock().await.len();
    let payload = serde_json::json!({
        "status": "ok",
        "generation": state.cfg.gen.as_int(),
        "board_mac": board_mac,
        "board_ip": board_ip,
        "uid": uid,
        "registered": uid.is_some(),
        "shutting_down": state.jobs.is_closing(),
        "deletion_requested": state.deletion_requested(),
        "usb_mapping_count": usb_mapping_count,
        "gen5_mapping_count": gen5_mapping_count,
        "active_capture_count": active_capture_count,
        "features": {
            "gen3": state.features.gen3,
            "gen4": state.features.gen4,
            "gen5": state.features.gen5,
            "rtos": state.features.rtos,
        }
    });
    Json(payload).into_response()
}

async fn metrics_handler() -> Response {
    let output = global_metrics().render();
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            crate::observability::metrics::PROMETHEUS_CONTENT_TYPE,
        )],
        output,
    )
        .into_response()
}

fn ok() -> Json<serde_json::Value> {
    debug!("generic success response generated");
    Json(serde_json::json!({"OK": true}))
}

fn err_json(msg: &str, code: StatusCode) -> (StatusCode, Json<serde_json::Value>) {
    debug!(status = %code, error = %msg, "error JSON response created");
    (code, Json(serde_json::json!({ "error": msg })))
}

fn error_response(message: impl AsRef<str>, status: StatusCode) -> Response {
    let message = message.as_ref();
    warn!(status = %status, error = %message, "API request failed and returned an error response");
    (
        status,
        Json(serde_json::json!({
            "error": message
        })),
    )
        .into_response()
}

macro_rules! log_handler_request {
    ($handler:literal, $request:expr) => {
        let _ = &$request;
        info!(handler = $handler, "API handler invoked");
    };
}

pub fn router(state: Arc<AppState>) -> Router {
    let mut router = Router::new()
        .route("/health", get(health_handler))
        .route("/ready", get(readiness_handler))
        .route("/status", get(status_handler))
        .route("/metrics", get(metrics_handler))
        .route(
            "/swagger.json",
            get(crate::observability::swagger::swagger_json),
        )
        .route("/docs", get(crate::observability::swagger::swagger_ui))
        .route("/confirmation", post(confirmation));

    if state.features.gen3 || state.features.gen4 {
        router = router
            .route("/relay", post(relay::relay_handler))
            .route("/relay/status", post(relay::relay_status_handler))
            .route("/relay/identity", post(relay::relay_identity_handler))
            .route("/relay/config", post(relay::relay_config_handler))
            .route("/relay/delete", post(relay::relay_delete_handler))
            .route("/devCon/delete", post(relay::devcon_delete_handler))
            .route("/ipl-mode", post(gen4::ipl_mode_handler))
            .route("/ipl-mode/default", post(gen4::ipl_mode_default_handler));
    }

    if state.features.gen3 || state.features.gen4 || state.features.gen5 {
        router = router
            .route("/ipl", post(gen4::ipl_run_handler))
            .route("/ipl/remove", post(gen4::ipl_remove_handler));
    }

    if state.features.gen5 {
        router = router
            .route("/gen5/tty_entry", post(gen5::gen5_tty_entry_handler))
            .route("/gen5/power", post(gen5::gen5_power_handler))
            .route("/mapping/entry", post(gen5::mapping_entry_handler))
            .route("/reboot-device", post(gen4::reboot_device_handler));
    }

    if state.features.rtos {
        router = router
            .route("/rtos/start", post(rtos::rtos_start_handler))
            .route("/rtos/end", post(rtos::rtos_end_handler));
    }

    router
        .layer(middleware::from_fn_with_state(state.clone(), request_guard))
        .with_state(state)
}

fn required<T>(value: Option<T>, field: &str) -> crate::error::AppResult<T> {
    value.ok_or_else(|| {
        warn!(field = %field, "required request value missing");
        crate::error::AppError::Msg(format!("{field} is required"))
    })
}

struct FlashWriterTargetRequest {
    generation: u8,
    mac: String,
    serial: String,
    channel: u8,
    gpio: u32,
    gpio_default_level: crate::models::VoltageLevel,
    relay_default_level: crate::models::VoltageLevel,
}

async fn flashwriter_target(
    state: &AppState,
    request: FlashWriterTargetRequest,
) -> crate::error::AppResult<ipl::FlashWriterTarget> {
    let FlashWriterTargetRequest {
        generation,
        mac,
        serial,
        channel,
        gpio,
        gpio_default_level,
        relay_default_level,
    } = request;
    if !ipl::supports_relay_flash(generation) || !state.features.generation_enabled(generation) {
        return Err(crate::error::AppError::Msg(format!(
            "Gen{generation} relay flashing is not enabled"
        )));
    }

    let mac = validated_mac(&mac)?;
    validate_csv_field("serial", &serial)?;
    let approved = state.hardware.board(&mac, generation)?;
    let approved_relay = approved
        .relay
        .as_ref()
        .ok_or_else(|| crate::error::AppError::Msg("approved relay binding missing".into()))?;

    if state.relay.identity()?.serial_number != serial
        || approved_relay.channel != channel
        || approved.gpio != Some(gpio)
    {
        return Err(crate::error::AppError::Msg(
            "relay or GPIO request differs from approved board wiring".into(),
        ));
    }

    let approved_tty = crate::usb::resolve_identity(&approved.uart)?;

    if channel > 7 {
        return Err(crate::error::AppError::Msg("channel must be 0..7".into()));
    }

    let tty = state
        .usb_map
        .read()
        .await
        .get(&(mac.clone(), serial.clone(), channel))
        .cloned()
        .ok_or_else(|| crate::error::AppError::Msg("UART mapping not found".into()))?;

    let tty = checked_tty(&tty)?;
    if tty != approved_tty {
        return Err(crate::error::AppError::Msg(
            "stored UART mapping is stale; verify the board mapping again".into(),
        ));
    }

    Ok(ipl::FlashWriterTarget {
        generation,
        tty,
        mac,
        serial,
        channel,
        gpio,
        gpio_default_level,
        relay_default_level,
    })
}

enum PreparedFlash {
    FlashWriter(ipl::FlashWriterTarget, std::path::PathBuf),
    Gen5(ipl::Gen5Job),
}

async fn prepare_flash(
    state: &AppState,
    request: IplRequest,
) -> crate::error::AppResult<PreparedFlash> {
    let mac = validated_mac(&required(request.mac, "mac")?)?;
    let path = required(request.path, "path")?;

    match request.gen {
        generation @ (3 | 4) => {
            let target = flashwriter_target(
                state,
                FlashWriterTargetRequest {
                    generation,
                    mac,
                    serial: required(request.serial, "serial")?,
                    channel: required(request.channel, "channel")?,
                    gpio: required(request.gpio, "gpio")?,
                    gpio_default_level: required(request.gpio_default_level, "gpioDefaultLevel")?,
                    relay_default_level: required(
                        request.relay_default_level,
                        "relayDefaultLevel",
                    )?,
                },
            )
            .await?;

            let package = task::spawn_blocking(move || {
                let package = ipl::package_directory(&path)?;
                ipl::preflight_flashwriter(&package)?;
                Ok::<_, crate::error::AppError>(package)
            })
            .await
            .map_err(|error| crate::error::AppError::Msg(error.to_string()))??;

            Ok(PreparedFlash::FlashWriter(target, package))
        }
        5 => {
            let uart = checked_tty(&required(request.uart, "uart")?)?;
            let power = checked_tty(&required(request.power, "power")?)?;
            let approved = state.hardware.board(&mac, 5)?;

            let approved_power = approved.power.as_ref().ok_or_else(|| {
                crate::error::AppError::Msg("approved Gen5 power binding missing".into())
            })?;

            if crate::usb::resolve_identity(&approved.uart)? != uart
                || crate::usb::resolve_identity(approved_power)? != power
            {
                return Err(crate::error::AppError::Msg(
                    "requested Gen5 devices differ from approved hardware identities".into(),
                ));
            }

            let sdk_version = required(request.sdk_ver, "sdk_ver")?;

            if uart == power
                || sdk_version.is_empty()
                || sdk_version.len() > 128
                || sdk_version.starts_with('-')
                || sdk_version.chars().any(char::is_control)
            {
                return Err(crate::error::AppError::Msg(
                    "invalid Gen5 UART, power device, or SDK version".into(),
                ));
            }
            let (package, script) = task::spawn_blocking(move || {
                let package = ipl::package_directory(&path)?;
                let script = ipl::gen5_script()?;

                // Validate required package structure before accepting work.
                let hil = fs::canonicalize(package.join("HIL"))?;
                if !hil.starts_with(&package) || !hil.is_dir() {
                    return Err(crate::error::AppError::Msg(
                        "Gen5 package must contain a HIL directory".into(),
                    ));
                }

                Ok::<_, crate::error::AppError>((package, script))
            })
            .await
            .map_err(|error| {
                crate::error::AppError::Msg(format!("firmware preflight worker failed: {error}"))
            })??;

            // Require the supplied devices to match this MAC's stored
            // mapping. A character-device check alone does not establish
            // that the device belongs to the requested board.
            let existing = state
                .gen5_map
                .read()
                .await
                .get(&mac)
                .cloned()
                .ok_or_else(|| {
                    crate::error::AppError::Msg(
                        "Gen5 mapping missing; run /gen5/tty_entry first".into(),
                    )
                })?;

            if checked_tty(&existing.uart)? != uart || checked_tty(&existing.power)? != power {
                return Err(crate::error::AppError::Msg(
                    "supplied Gen5 devices do not match the stored MAC mapping".into(),
                ));
            }

            Ok(PreparedFlash::Gen5(ipl::Gen5Job {
                package,
                script,
                sdk_version,
                uart,
                power,
                mac,
            }))
        }
        _ => Err(crate::error::AppError::Msg(
            "IPL supports generation 3, 4, or 5".into(),
        )),
    }
}

/// Accept an exclusive, tracked flash job.
///
/// HTTP 202 means accepted, not successfully flashed. Final status is sent
/// through the generation-specific backend callback and written to logs.
async fn ipl_run(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<IplRequest>,
) -> Response {
    log_handler_request!("ipl_run", &request);

    let prepared = match prepare_flash(&state, request).await {
        Ok(prepared) => prepared,
        Err(error) => {
            return error_response(error.to_string(), StatusCode::BAD_REQUEST);
        }
    };

    let worker_state = state.clone();

    state.jobs.spawn(lease, async move {
        let result = match prepared {
            PreparedFlash::FlashWriter(target, package) => {
                ipl::run_flashwriter(worker_state, target, package).await
            }
            PreparedFlash::Gen5(job) => ipl::run_gen5(worker_state, job).await,
        };

        if let Err(error) = result {
            error!(%error, "IPL job failed");
        }
    });

    info!(handler = "ipl_run", status = %StatusCode::ACCEPTED, "IPL request accepted for processing");

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "OK": true,
            "status": "accepted"
        })),
    )
        .into_response()
}

async fn execute_ipl_mode(
    state: Arc<AppState>,
    lease: Lease,
    request: IplModeRequest,
    download: bool,
) -> Response {
    let generation = state.cfg.gen.as_int();
    let target = match flashwriter_target(
        &state,
        FlashWriterTargetRequest {
            generation,
            mac: request.mac,
            serial: request.serial,
            channel: request.channel,
            gpio: request.gpio,
            gpio_default_level: request.gpio_default_level,
            relay_default_level: request.relay_default_level,
        },
    )
    .await
    {
        Ok(target) => target,
        Err(error) => {
            return error_response(error.to_string(), StatusCode::BAD_REQUEST);
        }
    };

    let result = task::spawn_blocking(move || {
        // Retain the lease in the blocking worker itself. Dropping an
        // awaiting HTTP future must not release hardware ownership while
        // the blocking operation continues.
        let _lease = lease;
        ipl::mode_sync(&state, &target, download)
    })
    .await;

    match result {
        Ok(Ok(())) => ok().into_response(),
        Ok(Err(error)) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
        Err(error) => error_response(
            format!("boot-mode worker failed: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

async fn ipl_mode(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<IplModeRequest>,
) -> Response {
    log_handler_request!("ipl_mode", &request);
    execute_ipl_mode(state, lease, request, true).await
}

async fn ipl_mode_default(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<IplModeRequest>,
) -> Response {
    log_handler_request!("ipl_mode_default", &request);
    execute_ipl_mode(state, lease, request, false).await
}

/// Power-cycle a mapped Gen5 board.
///
/// Success means both commands were written successfully. The supplied
/// power protocol does not define an acknowledgment or boot-health check,
/// so this endpoint cannot establish that the board actually booted.
async fn reboot_device(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<RebootDeviceRequest>,
) -> Response {
    log_handler_request!("reboot_device", &request);

    let power = match mapped_power_tty(&state, &request.power).await {
        Ok(power) => power,
        Err(error) => {
            return error_response(error.to_string(), StatusCode::BAD_REQUEST);
        }
    };

    let result = task::spawn_blocking(move || {
        let _lease = lease;

        write_to_path(&power, "POWER#OFF\n")?;
        std::thread::sleep(Duration::from_secs(2));

        // Do not retry blindly: without acknowledgments, the result of a
        // failed write can be ambiguous.
        write_to_path(&power, "POWER#ON\n").map_err(|error| {
            crate::error::AppError::Msg(format!(
                "power-off was sent, but power-on failed; \
                 inspect the board's power state: {error}"
            ))
        })
    })
    .await;

    match result {
        Ok(Ok(())) => {
            info!(handler = "reboot_device", status = %StatusCode::OK, "device reboot command completed");
            ok().into_response()
        }
        Ok(Err(error)) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
        Err(error) => error_response(
            format!("reboot worker failed: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

async fn confirmation(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ConfirmationRequest>,
) -> Response {
    log_handler_request!("confirmation", &request);

    if let Err(error) = crate::store::validate_uid(&request.controller_id) {
        return error_response(error.to_string(), StatusCode::BAD_REQUEST);
    }

    match state.save_uid(&request.controller_id).await {
        Ok(()) => {
            info!(handler = "confirmation", status = %StatusCode::OK, "controller confirmation processed");
            ok().into_response()
        }
        Err(error) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Delete mappings while holding the same lock used by discovery.
///
/// All supplied selectors must match a record. With no selectors, delete
/// all records for the requested generation.
fn delete_mappings_sync(
    state: Arc<AppState>,
    generation: Generation,
    mac: Option<String>,
    serial: Option<String>,
    channel: Option<u8>,
) -> crate::error::AppResult<()> {
    use crate::error::AppError;

    let mac = mac.as_deref().map(validated_mac).transpose()?;

    if let Some(serial) = serial.as_deref() {
        validate_csv_field("relay serial", serial)?;
    }

    if channel.is_some_and(|channel| channel > 7) {
        return Err(AppError::Msg("relay channel must be 0..7".into()));
    }

    let _operation = mapping_guard()?;

    match generation {
        Generation::Gen3 | Generation::Gen4 => {
            let mut updated = state.usb_map.blocking_read().clone();

            updated.retain(|(stored_mac, stored_serial, stored_channel), _| {
                let matches = mac.as_ref().is_none_or(|value| value == stored_mac)
                    && serial.as_ref().is_none_or(|value| value == stored_serial)
                    && channel.is_none_or(|value| value == *stored_channel);

                !matches
            });

            persist_usb_mappings(&state, &updated)?;
            *state.usb_map.blocking_write() = updated;
        }
        Generation::Gen5 => {
            if serial.is_some() || channel.is_some() {
                return Err(AppError::Msg(
                    "Gen5 deletion does not accept relay selectors".into(),
                ));
            }

            let mut updated = state.gen5_map.blocking_read().clone();

            match mac {
                Some(mac) => {
                    updated.remove(&mac);
                }
                None => updated.clear(),
            }

            persist_gen5_mappings(&state, &updated)?;
            *state.gen5_map.blocking_write() = updated;
        }
    }

    Ok(())
}

async fn devcon_delete(
    State(state): State<Arc<AppState>>,
    Json(request): Json<DeleteRequest>,
) -> Response {
    log_handler_request!("devcon_delete", &request);

    let Some(generation) = Generation::from_int(request.gen) else {
        return error_response("invalid generation", StatusCode::BAD_REQUEST);
    };

    let full_delete =
        request.mac.is_none() && request.serial.is_none() && request.channel.is_none();

    // Prevent a partial selector set from accidentally unregistering
    // the entire controller.
    if request.mac.is_none() && !full_delete {
        return error_response(
            "selective controller deletion requires mac",
            StatusCode::BAD_REQUEST,
        );
    }

    if full_delete {
        let supplied_uid = match request.uid.as_deref() {
            Some(uid) if !uid.is_empty() => uid,
            _ => {
                return error_response("full deletion requires uid", StatusCode::BAD_REQUEST);
            }
        };

        let controller = state.controller.read().await;

        if controller.uid.as_deref() != Some(supplied_uid) {
            return error_response("controller UID does not match", StatusCode::CONFLICT);
        }
    }

    let worker_state = state.clone();

    let result = task::spawn_blocking(move || {
        delete_mappings_sync(
            worker_state,
            generation,
            request.mac,
            request.serial,
            request.channel,
        )
    })
    .await;

    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            return error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
        Err(error) => {
            return error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    if full_delete {
        if let Err(error) = state.clear_uid().await {
            return error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }

        state.request_deletion();
    }

    info!(handler = "devcon_delete", status = %StatusCode::OK, "device mapping deletion completed");
    ok().into_response()
}

async fn relay_delete(
    State(state): State<Arc<AppState>>,
    Json(request): Json<DeleteRequest>,
) -> Response {
    log_handler_request!("relay_delete", &request);

    let Some(generation @ (Generation::Gen3 | Generation::Gen4)) =
        Generation::from_int(request.gen)
    else {
        return error_response(
            "relay deletion requires gen 3 or 4",
            StatusCode::BAD_REQUEST,
        );
    };

    if request.mac.is_none() && request.serial.is_none() {
        return error_response("provide mac or relay serial", StatusCode::BAD_REQUEST);
    }

    if request.serial.is_none() && request.channel.is_some() {
        return error_response("channel requires relay serial", StatusCode::BAD_REQUEST);
    }

    let result = task::spawn_blocking(move || {
        delete_mappings_sync(
            state,
            generation,
            request.mac,
            request.serial,
            request.channel,
        )
    })
    .await;

    match result {
        Ok(Ok(())) => {
            info!(handler = "relay_delete", status = %StatusCode::OK, "relay mapping deletion completed");
            ok().into_response()
        }
        Ok(Err(error)) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
        Err(error) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn gen5_tty_entry(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Gen5TtyRequest>,
) -> impl IntoResponse {
    log_handler_request!("gen5_tty_entry", &req);

    match resolve_gen5_mapping_sync(state.clone(), &req.mac) {
        Ok(Some((uart, power))) => {
            let body = serde_json::json!({ "uart": uart, "power": power });
            info!(handler = "gen5_tty_entry", uart = %uart, power = %power, "Gen5 TTY mapping resolved");
            let _ = notify_gen5_mapping(&state, &req.mac, Some(&body)).await;
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        }
        Ok(None) => {
            let _ = notify_gen5_mapping(&state, &req.mac, None).await;
            err_json("Not found", StatusCode::BAD_REQUEST).into_response()
        }
        Err(e) => err_json(&e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response(),
    }
}

async fn relay(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<RelayRequest>,
) -> Response {
    log_handler_request!("relay", &request);

    if request.channel > 7 {
        return error_response("relay channel must be 0..7", StatusCode::BAD_REQUEST);
    }

    if let Err(error) = validate_csv_field("serial", &request.serial) {
        return error_response(error.to_string(), StatusCode::BAD_REQUEST);
    }

    let on = match parse_power_state(&request.state) {
        Ok(on) => on,
        Err(error) => {
            return error_response(error.to_string(), StatusCode::BAD_REQUEST);
        }
    };

    if !state.hardware.relay_allowed(request.channel) {
        return error_response(
            "relay/channel is not approved by hardware policy",
            StatusCode::FORBIDDEN,
        );
    }

    let relay_serial = request.serial.clone();
    let relay_state = request.state.clone();
    let relay_channel = request.channel;

    let result = task::spawn_blocking(move || {
        let _lease = lease;
        state
            .relay
            .set_channel(&request.serial, request.channel, on)
    })
    .await;

    match result {
        Ok(Ok(())) => {
            info!(handler = "relay", serial = %relay_serial, channel = relay_channel, state = %relay_state, "relay state change applied");
            ok().into_response()
        }
        Ok(Err(error)) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
        Err(error) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn relay_status(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<RelayStatusRequest>,
) -> Response {
    log_handler_request!("relay_status", &request);

    if request.channel > 7 {
        return error_response("relay channel must be 0..7", StatusCode::BAD_REQUEST);
    }

    if let Err(error) = validate_csv_field("serial", &request.serial) {
        return error_response(error.to_string(), StatusCode::BAD_REQUEST);
    }

    if !state.hardware.relay_allowed(request.channel) {
        return error_response(
            "relay/channel is not approved by hardware policy",
            StatusCode::FORBIDDEN,
        );
    }

    let relay_serial = request.serial.clone();
    let relay_channel = request.channel;

    let result = task::spawn_blocking(move || {
        let _lease = lease;
        state.relay.channel_status(&request.serial, request.channel)
    })
    .await;

    match result {
        Ok(Ok(value)) => {
            info!(handler = "relay_status", serial = %relay_serial, channel = relay_channel, state = value, "relay status retrieved");
            Json(serde_json::json!({ "state": value })).into_response()
        }
        Ok(Err(error)) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
        Err(error) => error_response(
            format!("relay status worker failed: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

async fn relay_identity(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<RelayIdentityUpdateRequest>,
) -> Response {
    log_handler_request!("relay_identity", &request);

    if request.vid_pid.trim().is_empty() {
        return error_response(
            "vidPid is required; serialNumber is optional",
            StatusCode::BAD_REQUEST,
        );
    }

    let result = task::spawn_blocking(move || {
        let _lease = lease;
        let _mapping = mapping_guard()?;
        let identity = crate::relay::RelayController::resolve(crate::relay::RelaySelector {
            serial_number: request.serial_number,
            vid_pid: Some(request.vid_pid),
        })?;
        let previous = state.relay.identity()?;
        let mut updated = UsbMappings::new();
        for ((mac, serial, channel), tty) in state.usb_map.blocking_read().clone() {
            let serial = if serial == previous.serial_number {
                identity.serial_number.clone()
            } else {
                serial
            };
            if updated.insert((mac, serial, channel), tty).is_some() {
                return Err(crate::error::AppError::Msg(
                    "relay identity update would create duplicate mappings".into(),
                ));
            }
        }
        persist_usb_mappings(&state, &updated)?;
        *state.usb_map.blocking_write() = updated;
        state.relay.update_identity(identity.clone())?;
        Ok::<_, crate::error::AppError>(identity)
    })
    .await;

    match result {
        Ok(Ok(identity)) => Json(serde_json::json!({
            "OK": true,
            "relay": identity,
        }))
        .into_response(),
        Ok(Err(error)) => error_response(error.to_string(), StatusCode::BAD_REQUEST),
        Err(error) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn relay_config(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RelayConfigRequest>,
) -> Response {
    log_handler_request!("relay_config", &request);

    let Some(generation) = Generation::from_int(request.gen) else {
        return error_response("invalid generation", StatusCode::BAD_REQUEST);
    };

    if generation == Generation::Gen5 {
        return error_response(
            "relay configuration applies only to Gen3 and Gen4",
            StatusCode::BAD_REQUEST,
        );
    }

    let state_copy = state.clone();
    let mac = request.mac.clone();
    let serial = request.serial.clone();
    let channel = request.channel;

    let mapped = task::spawn_blocking(move || {
        map_usb_to_mac_sync(state_copy, mac, serial, channel, generation)
    })
    .await;

    let success = matches!(mapped, Ok(Ok(true)));

    let _ = notify_relay_config(&state, &request.mac, success).await;

    if success {
        info!(handler = "relay_config", mac = %request.mac, serial = %request.serial, channel = request.channel, generation = request.gen, "relay configuration persisted");
        ok().into_response()
    } else {
        error_response(
            "could not associate UART with requested MAC",
            StatusCode::BAD_REQUEST,
        )
    }
}

// async fn gen5_tty_entry(
//     State(state): State<Arc<AppState>>,
//     Json(request): Json<Gen5TtyRequest>,
// ) -> Response {
//     let mac = request.mac.clone();
//     let worker_state = state.clone();
//
//     let result = task::spawn_blocking(move || resolve_gen5_mapping_sync(worker_state, mac)).await;
//
//     match result {
//         Ok(Ok(Some((uart, power)))) => {
//             let payload = serde_json::json!({
//                 "uart": uart,
//                 "power": power
//             });
//
//             let _ = notify_gen5_mapping(&state, &request.mac, Some(&payload)).await;
//
//             Json(payload).into_response()
//         }
//         Ok(Ok(None)) => {
//             let _ = notify_gen5_mapping(&state, &request.mac, None).await;
//             error_response("mapping not found", StatusCode::BAD_REQUEST)
//         }
//         Ok(Err(e)) => {
//             let _ = notify_gen5_mapping(&state, &request.mac, None).await;
//             error_response(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
//         }
//         Err(e) => error_response(
//             format!("mapping worker failed: {e}"),
//             StatusCode::INTERNAL_SERVER_ERROR,
//         ),
//     }
// }
//
async fn gen5_power(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<Gen5PowerRequest>,
) -> Response {
    log_handler_request!("gen5_power", &request);

    let on = match parse_power_state(&request.state) {
        Ok(on) => on,
        Err(error) => {
            return error_response(error.to_string(), StatusCode::BAD_REQUEST);
        }
    };

    let power = match mapped_power_tty(&state, &request.power).await {
        Ok(power) => power,
        Err(error) => {
            return error_response(error.to_string(), StatusCode::BAD_REQUEST);
        }
    };

    let result = task::spawn_blocking(move || {
        let _lease = lease;

        write_to_path(&power, if on { "POWER#ON\n" } else { "POWER#OFF\n" })
    })
    .await;

    match result {
        Ok(Ok(())) => {
            info!(handler = "gen5_power", power = %request.power, state = %request.state, "Gen5 power transaction completed");
            ok().into_response()
        }
        Ok(Err(error)) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
        Err(error) => error_response(
            format!("power-control worker failed: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

async fn mapping_entry(
    State(_state): State<Arc<AppState>>,
    Json(request): Json<MappingEntryRequest>,
) -> Response {
    log_handler_request!("mapping_entry", &request);

    if request.gen != 5 {
        return error_response(
            "TTY inventory discovery requires gen 5",
            StatusCode::BAD_REQUEST,
        );
    }

    let result = task::spawn_blocking(|| {
        let _operation = mapping_guard()?;
        discover_gen5_ttys()
    })
    .await;

    match result {
        Ok(Ok((uart, power))) => {
            info!(handler = "mapping_entry", uart = ?uart, power = ?power, "Gen5 mapping entry discovered");
            Json(serde_json::json!({
                "uart": uart,
                "power": power,
            }))
            .into_response()
        }
        Ok(Err(error)) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
        Err(error) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn ipl_remove(
    State(_state): State<Arc<AppState>>,
    Json(request): Json<RemoveIplRequest>,
) -> Response {
    log_handler_request!("ipl_remove", &request);

    let result = task::spawn_blocking(move || -> crate::error::AppResult<()> {
        use crate::error::AppError;
        use std::path::Component;

        let supplied = Path::new(&request.path);

        if !supplied.is_absolute()
            || supplied
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(AppError::Msg(
                "expected an absolute path without '..'".into(),
            ));
        }

        let root = fs::canonicalize(FIRMWARE_ROOT)?;
        let target = fs::canonicalize(supplied)?;

        // Never allow deletion of the storage root itself.
        if target == root || !target.starts_with(&root) {
            return Err(AppError::Msg(
                "path is outside the firmware storage directory".into(),
            ));
        }

        if !target.is_dir() {
            return Err(AppError::Msg(
                "expected a firmware package directory".into(),
            ));
        }

        fs::remove_dir_all(target)?;
        Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => ok().into_response(),
        Ok(Err(e)) => error_response(e.to_string(), StatusCode::BAD_REQUEST),
        Err(e) => error_response(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Validate rather than merely normalize an externally supplied MAC.
///
/// Exact comparison avoids accepting a prefix of another device's MAC.
fn validated_mac(value: &str) -> crate::error::AppResult<String> {
    let result = crate::store::checked_mac(value);
    if let Err(error) = &result {
        warn!(mac = %value, %error, "MAC validation failed");
    } else {
        debug!(mac = %value, "MAC validation succeeded");
    }
    result
}

/// Resolve symlinks and restrict serial access to actual USB/ACM character
/// devices. This also permits /dev/serial/by-id/... aliases.
///
/// This is a device-type check, not authorization. A production deployment
/// should additionally allowlist the expected device identities.
fn checked_tty(value: &str) -> crate::error::AppResult<String> {
    use std::os::unix::fs::FileTypeExt;

    let path = match fs::canonicalize(value) {
        Ok(path) => path,
        Err(error) => {
            warn!(tty = %value, %error, "TTY canonicalization failed");
            return Err(error.into());
        }
    };

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| crate::error::AppError::Msg("invalid TTY path".into()))?;

    let recognized_name = ["ttyUSB", "ttyACM"].iter().any(|prefix| {
        name.strip_prefix(prefix).is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
    });

    if path.parent() != Some(Path::new("/dev"))
        || !recognized_name
        || !fs::metadata(&path)?.file_type().is_char_device()
    {
        warn!(tty = %value, canonical = %path.display(), recognized_name, "TTY validation rejected");
        return Err(crate::error::AppError::Msg(
            "expected a USB or ACM serial character device".into(),
        ));
    }

    let canonical = path
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| crate::error::AppError::Msg("non-UTF-8 TTY path".into()))?;

    debug!(tty = %value, canonical = %canonical, "TTY validation succeeded");
    Ok(canonical)
}

/// Preserve the legacy Gen4 RTOS mapping convention.
///
/// IMPORTANT: ttyUSB numbering is not a stable hardware identity.
/// A later migration should store the RTOS interface explicitly, ideally
/// using /dev/serial/by-id paths.
async fn resolve_rtos_tty(
    state: &AppState,
    generation: u8,
    mac: Option<&str>,
    serial: Option<&str>,
    channel: Option<u8>,
    explicit_tty: Option<&str>,
) -> crate::error::AppResult<String> {
    use crate::error::AppError;

    let mac = validated_mac(mac.ok_or_else(|| AppError::Msg("mac is required".into()))?)?;

    if !matches!(generation, 4 | 5) {
        return Err(AppError::Msg("RTOS capture supports Gen4 and Gen5".into()));
    }

    let board = state.hardware.board(&mac, generation)?;

    if generation == 4 {
        let relay = board
            .relay
            .as_ref()
            .ok_or_else(|| AppError::Msg("approved relay binding missing".into()))?;

        if serial != Some(state.relay.identity()?.serial_number.as_str())
            || channel != Some(relay.channel)
        {
            return Err(AppError::Msg(
                "RTOS request does not match approved relay binding".into(),
            ));
        }
    }

    let tty = state.hardware.rtos_device(&mac, generation)?;

    if let Some(supplied) = explicit_tty {
        if checked_tty(supplied)? != tty {
            return Err(AppError::Msg(
                "requested RTOS TTY differs from approved interface".into(),
            ));
        }
    }

    Ok(tty)
}

async fn rtos_start(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RtosStartRequest>,
) -> Response {
    log_handler_request!("rtos_start", &request);

    let session_key = match request.mac.as_deref().map(validated_mac).transpose() {
        Ok(Some(mac)) => mac,
        Ok(None) => {
            return error_response("mac is required", StatusCode::BAD_REQUEST);
        }
        Err(error) => {
            return error_response(error.to_string(), StatusCode::BAD_REQUEST);
        }
    };

    let tty = match resolve_rtos_tty(
        &state,
        request.gen,
        request.mac.as_deref(),
        request.serial.as_deref(),
        request.channel,
        request.rtos.as_deref(),
    )
    .await
    {
        Ok(tty) => tty,
        Err(e) => return error_response(e.to_string(), StatusCode::BAD_REQUEST),
    };

    // Serialize session lifecycle changes. The capture workers themselves
    // do not acquire this mutex, so waiting for their completion is safe.
    let mut sessions = state.rtos_sessions.lock().await;

    if let Some(session) = sessions.get(&session_key) {
        if session
            .handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
        {
            return ok().into_response();
        }

        // Preserve any finished capture until /rtos/end downloads it.
        return error_response(
            "capture finished; download it with /rtos/end before restarting",
            StatusCode::CONFLICT,
        );
    }

    if sessions.len() >= MAX_CAPTURE_SESSIONS {
        return error_response("capture session limit reached", StatusCode::CONFLICT);
    }

    let worker_tty = tty.clone();

    // Open resources before returning success. The endpoint must not claim
    // that capture started if the UART or output file could not be opened.
    let opened = task::spawn_blocking(move || {
        fs::create_dir_all(CAPTURE_ROOT)?;
        let port = open_uart(&worker_tty, 115_200)?;
        let file = tempfile::NamedTempFile::new_in(CAPTURE_ROOT)?;
        let writer = file.reopen()?;

        Ok::<_, crate::error::AppError>((port, file, writer))
    })
    .await;

    let (mut port, file, mut writer) = match opened {
        Ok(Ok(resources)) => resources,
        Ok(Err(e)) => {
            return error_response(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
        Err(e) => {
            return error_response(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();

    let handle = task::spawn_blocking(move || -> crate::error::AppResult<()> {
        let mut buffer = [0_u8; 4096];
        let mut written = 0_u64;

        while !worker_stop.load(Ordering::Acquire) {
            match port.read(&mut buffer) {
                Ok(0) => {
                    // Avoid spinning if a driver repeatedly returns zero.
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(count) => {
                    let remaining = MAX_CAPTURE_BYTES.saturating_sub(written);
                    let keep = count.min(remaining as usize);

                    writer.write_all(&buffer[..keep])?;
                    written += keep as u64;

                    if written >= MAX_CAPTURE_BYTES {
                        writer.flush()?;
                        return Err(crate::error::AppError::Msg(
                            "capture stopped at the configured size limit".into(),
                        ));
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e.into()),
            }
        }

        writer.flush()?;
        Ok(())
    });

    let session_key_for_log = session_key.clone();
    let tty_for_log = tty.clone();

    sessions.insert(
        session_key,
        RtosSession {
            tty,
            file,
            stop,
            handle: Some(handle),
        },
    );
    global_metrics().set_active_captures(sessions.len());

    info!(handler = "rtos_start", mac = %session_key_for_log, tty = %tty_for_log, "RTOS capture started");
    ok().into_response()
}

async fn rtos_end(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RtosEndRequest>,
) -> Response {
    log_handler_request!("rtos_end", &request);

    let session_key = match request.mac.as_deref().map(validated_mac).transpose() {
        Ok(Some(mac)) => mac,
        Ok(None) => {
            return error_response("mac is required", StatusCode::BAD_REQUEST);
        }
        Err(error) => {
            return error_response(error.to_string(), StatusCode::BAD_REQUEST);
        }
    };

    if let Err(error) = state.hardware.board(&session_key, request.gen) {
        return error_response(error.to_string(), StatusCode::BAD_REQUEST);
    }

    let mut sessions = state.rtos_sessions.lock().await;

    let Some(mut session) = sessions.remove(&session_key) else {
        return (StatusCode::OK, "").into_response();
    };
    global_metrics().set_active_captures(sessions.len());

    let tty = session.tty.clone();

    session.stop.store(true, Ordering::Release);

    let capture_status = match session.handle.take() {
        Some(handle) => match handle.await {
            Ok(Ok(())) => "complete",
            Ok(Err(e)) => {
                warn!(%tty, error = %e, "capture ended with an error");
                "partial"
            }
            Err(e) => {
                error!(%tty, error = %e, "capture worker failed");
                "partial"
            }
        },
        None => "partial",
    };

    // Reopen before dropping NamedTempFile. On Linux, unlinking the pathname
    // does not invalidate the open descriptor used by the response stream.
    let file = match tokio::fs::File::open(session.file.path()).await {
        Ok(file) => file,
        Err(e) => {
            return error_response(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    drop(session);
    drop(sessions);

    let stream = tokio_util::io::ReaderStream::new(file);

    info!(handler = "rtos_end", mac = %session_key, capture_status = %capture_status, "RTOS capture stop requested and download prepared");

    (
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"rtos.log\"",
            ),
            (
                axum::http::HeaderName::from_static("x-capture-status"),
                capture_status,
            ),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

/// Send the legacy relay-mapping confirmation payload.
async fn notify_relay_config(
    state: &AppState,
    mac: &str,
    success: bool,
) -> Result<(), reqwest::Error> {
    let url = format!(
        "http://{}:{}/api/v1/device/relay/config/confirmation",
        state.cfg.server_ip, state.cfg.http_port,
    );

    let payload = serde_json::json!({
        "mac": mac,
        "status": if success { "success" } else { "failure" },
    });

    tracing::info!(%url, mac = %mac, success, "sending relay-config confirmation to backend");

    let response = state.client.post(url).json(&payload).send().await?;
    let status = response.status();

    if !status.is_success() {
        tracing::warn!(%mac, %status, "relay-config confirmation rejected by backend");
        return Ok(());
    }

    tracing::debug!(%mac, %status, "relay-config confirmation accepted by backend");
    Ok(())
}

/// `tty_entry` is intentionally a JSON-encoded STRING.
///
/// This matches the original callback contract rather than silently changing
/// it into a nested JSON object.
async fn notify_gen5_mapping(
    state: &AppState,
    mac: &str,
    mapping: Option<&serde_json::Value>,
) -> Result<(), reqwest::Error> {
    let ip = state.controller.read().await.board_ip.clone();

    let mut payload = serde_json::json!({
        "mac": mac,
        "ip": ip,
        "status": if mapping.is_some() { "success" } else { "failure" },
    });

    if let Some(mapping) = mapping {
        payload["tty_entry"] = serde_json::Value::String(mapping.to_string());
    }

    let url = format!(
        "http://{}:{}/api/v1/device/mapping-gen5",
        state.cfg.server_ip, state.cfg.http_port,
    );

    tracing::info!(%url, mac = %mac, mapping_present = mapping.is_some(), "sending Gen5 mapping notification to backend");
    let response = state.client.post(url).json(&payload).send().await?;
    let status = response.status();

    if !status.is_success() {
        tracing::warn!(%mac, %status, "Gen5 mapping notification rejected by backend");
        return Ok(());
    }

    tracing::debug!(%mac, %status, "Gen5 mapping notification accepted by backend");
    Ok(())
}

/// Read U-Boot's ethaddr using a bounded byte buffer.
///
/// Matching bytes rather than slicing UTF-8 strings avoids panics when
/// serial noise includes invalid UTF-8. Each operation has a total deadline.
fn read_uboot_mac(
    port: &mut dyn serialport::SerialPort,
    timeout: Duration,
) -> crate::error::AppResult<Option<String>> {
    use std::time::Instant;

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|part| part == needle)
    }

    let deadline = Instant::now() + timeout;
    let mut accumulated = Vec::with_capacity(8192);
    let mut buffer = [0_u8; 512];
    let mut queried = false;

    while Instant::now() < deadline {
        match port.read(&mut buffer) {
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Ok(count) => accumulated.extend_from_slice(&buffer[..count]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        }

        if !queried {
            if contains(&accumulated, b"Hit any key to stop autoboot") {
                port.write_all(b"\r")?;
                port.flush()?;
                accumulated.clear();
                continue;
            }

            if contains(&accumulated, b"TFTP from server") {
                // Interrupt a network boot so U-Boot returns to its prompt.
                // Write individual Ctrl-C bytes, matching the board protocol.
                for _ in 0..5 {
                    port.write_all(b"\x03")?;
                    port.flush()?;
                    std::thread::sleep(Duration::from_millis(5));
                }

                accumulated.clear();
                continue;
            }

            if contains(&accumulated, b"=> ") {
                port.write_all(b"printenv ethaddr\r\n")?;
                port.flush()?;

                queried = true;
                accumulated.clear();
                continue;
            }
        }

        if queried {
            if let Some(mac) = extract_uboot_mac(&accumulated)? {
                return Ok(Some(mac));
            }

            // A new prompt without ethaddr means the command completed,
            // but the environment variable was not present.
            if contains(&accumulated, b"=> ") {
                return Ok(None);
            }
        }

        // Bound memory consumption while retaining enough trailing bytes
        // to recognize prompts and MAC addresses split across UART reads.
        //
        // Work on bytes: serial noise need not be valid UTF-8.
        if accumulated.len() > 8192 {
            let discard = accumulated.len() - 4096;
            accumulated.drain(..discard);
        }
    }

    // No complete MAC response arrived before the total deadline.
    Ok(None)
}

/// Extract a MAC only from a complete `ethaddr=...` response line.
///
/// Returning `None` for an incomplete line lets the caller accumulate the
/// next UART read. A complete but malformed value is an error rather than
/// a partial or prefix match.
fn extract_uboot_mac(response: &[u8]) -> crate::error::AppResult<Option<String>> {
    use crate::error::AppError;

    for terminated_line in response.split_inclusive(|byte| *byte == b'\r' || *byte == b'\n') {
        let terminated = terminated_line
            .last()
            .is_some_and(|byte| *byte == b'\r' || *byte == b'\n');

        if !terminated {
            // The final line may be split across multiple serial reads.
            continue;
        }

        let line = &terminated_line[..terminated_line.len() - 1];

        // Ignore unrelated output, including the echoed command.
        let Some(value) = line.strip_prefix(b"ethaddr=") else {
            continue;
        };

        let value = std::str::from_utf8(value)
            .map_err(|_| AppError::Msg("U-Boot returned non-UTF-8 ethaddr data".into()))?;

        return validated_mac(value.trim()).map(Some);
    }

    Ok(None)
}

fn parse_power_state(value: &str) -> crate::error::AppResult<bool> {
    if value.eq_ignore_ascii_case("on") {
        debug!(state = %value, parsed = true, "power state parsed as ON");
        Ok(true)
    } else if value.eq_ignore_ascii_case("off") {
        debug!(state = %value, parsed = false, "power state parsed as OFF");
        Ok(false)
    } else {
        warn!(state = %value, "invalid power state received; expected on/off");
        Err(crate::error::AppError::Msg(
            "state must be 'on' or 'off'".into(),
        ))
    }
}

/// Allow direct power commands only for a mapped Gen5 power controller.
///
/// Device-node validation alone would still permit writes to an unrelated
/// USB serial device. This adds a mapping-based authorization boundary.
///
/// Stored ttyUSB names can become stale after hotplug; re-run mapping after
/// USB topology changes.
async fn mapped_power_tty(state: &AppState, supplied: &str) -> crate::error::AppResult<String> {
    let supplied = checked_tty(supplied)?;
    tracing::debug!(supplied_tty = %supplied, "validating supplied Gen5 power TTY");
    state.hardware.power_allowed(&supplied)?;

    let entries: Vec<String> = state
        .gen5_map
        .read()
        .await
        .values()
        .map(|entry| entry.power.clone())
        .collect();

    let permitted = entries.iter().any(|stored| {
        checked_tty(stored)
            .map(|stored| stored == supplied)
            .unwrap_or(false)
    });

    if !permitted {
        warn!(supplied_tty = %supplied, mapped_count = entries.len(), "power device is not present in a stored Gen5 mapping");
        return Err(crate::error::AppError::Msg(
            "power device is not present in a stored Gen5 mapping".into(),
        ));
    }

    info!(supplied_tty = %supplied, "Gen5 power TTY authorized and mapped");
    Ok(supplied)
}

#[cfg(test)]
mod tests {
    use super::{extract_uboot_mac, readiness_status, router, validated_mac};
    use crate::{
        config::AppConfig,
        hardware::HardwarePolicy,
        jobs::Jobs,
        models::Generation,
        relay::RelayController,
        state::{AppState, ControllerInfo},
        FeatureFlags,
    };
    use std::{
        collections::HashMap,
        sync::{atomic::AtomicBool, Arc},
    };
    use tokio::sync::{Mutex, RwLock};

    fn test_state(features: FeatureFlags, generation: Generation) -> Arc<AppState> {
        Arc::new(AppState {
            cfg: AppConfig {
                server_ip: "127.0.0.1".into(),
                http_port: 1,
                ws_port: 1,
                gen: generation,
                bind_port: 8888,
                iface_name: "eth0".into(),
            },
            controller: RwLock::new(ControllerInfo {
                board_mac: "001122334455".into(),
                board_ip: "127.0.0.1".into(),
                uid: None,
            }),
            usb_map: RwLock::new(HashMap::new()),
            gen5_map: RwLock::new(HashMap::new()),
            rtos_sessions: Mutex::new(HashMap::new()),
            relay: RelayController::new(None).unwrap(),
            deletion_requested: AtomicBool::new(false),
            client: reqwest::Client::new(),
            jobs: Jobs::default(),
            api_token: None,
            hardware: HardwarePolicy { boards: Vec::new() },
            features,
            registration_done: AtomicBool::new(false),
        })
    }

    async fn test_server(
        features: FeatureFlags,
        generation: Generation,
    ) -> (String, tokio::task::JoinHandle<()>) {
        test_server_with_token(features, generation, None).await
    }

    async fn test_server_with_token(
        features: FeatureFlags,
        generation: Generation,
        api_token: Option<&str>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut state = test_state(features, generation);
        Arc::get_mut(&mut state).unwrap().api_token = api_token.map(str::to_owned);
        let app = router(state);
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn normalizes_supported_mac_formats() {
        for input in ["AA:BB:CC:DD:EE:FF", "AA-BB-CC-DD-EE-FF", "aabbccddeeff"] {
            assert_eq!(validated_mac(input).unwrap(), "aabbccddeeff");
        }
    }

    #[test]
    fn rejects_short_long_and_non_hex_addresses() {
        for input in ["", "aa:bb:cc", "aa:bb:cc:dd:ee:ff:00", "aa:bb:cc:dd:ee:gg"] {
            assert!(validated_mac(input).is_err());
        }
    }

    #[test]
    fn extracts_complete_environment_response() {
        let response = b"printenv ethaddr\r\nethaddr=AA:BB:CC:DD:EE:FF\r\n=> ";

        assert_eq!(
            extract_uboot_mac(response).unwrap(),
            Some("aabbccddeeff".to_owned())
        );
    }

    #[test]
    fn waits_for_line_termination() {
        assert_eq!(
            extract_uboot_mac(b"ethaddr=aa:bb:cc:dd:ee:ff").unwrap(),
            None
        );
    }

    #[test]
    fn ignores_command_echo_and_unrelated_output() {
        let response = b"\xff\xfe boot noise\r\nprintenv ethaddr\r\n=> ";

        assert_eq!(extract_uboot_mac(response).unwrap(), None);
    }

    #[test]
    fn rejects_malformed_environment_value() {
        assert!(extract_uboot_mac(b"ethaddr=aa:bb:cc\r\n").is_err());
    }

    #[test]
    fn does_not_accept_a_different_variable() {
        assert_eq!(
            extract_uboot_mac(b"other_ethaddr=aa:bb:cc:dd:ee:ff\r\n").unwrap(),
            None
        );
    }

    #[test]
    fn readiness_requires_registration_and_an_open_job_gate() {
        assert_eq!(readiness_status(true, false), axum::http::StatusCode::OK);
        assert_eq!(
            readiness_status(false, false),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            readiness_status(true, true),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn gen3_feature_exposes_shared_ipl_without_enabling_gen5() {
        let (base, server) = test_server(
            FeatureFlags {
                gen3: true,
                ..FeatureFlags::default()
            },
            Generation::Gen3,
        )
        .await;
        let response = reqwest::Client::new()
            .post(format!("{base}/ipl"))
            .json(&serde_json::json!({"gen": 3}))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(response.text().await.unwrap().contains("mac is required"));
        assert!(crate::observability::metrics::global_metrics()
            .render()
            .contains(
                "edgecontroller_hardware_operations_total{operation=\"ipl\",outcome=\"failure\"}"
            ));
        server.abort();
    }

    #[tokio::test]
    async fn gen5_mapping_route_is_rejected_when_only_gen3_is_enabled() {
        let (base, server) = test_server(
            FeatureFlags {
                gen3: true,
                ..FeatureFlags::default()
            },
            Generation::Gen3,
        )
        .await;
        let response = reqwest::Client::new()
            .post(format!("{base}/mapping/entry"))
            .json(&serde_json::json!({"gen": 5, "mac": "001122334455"}))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::NOT_IMPLEMENTED);
        server.abort();
    }

    #[tokio::test]
    async fn unknown_paths_use_a_bounded_metrics_label() {
        let (base, server) = test_server(FeatureFlags::default(), Generation::Gen4).await;
        let unique_path = "/unknown-path-that-must-not-be-a-label";
        let response = reqwest::get(format!("{base}{unique_path}")).await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);

        let metrics = crate::observability::metrics::global_metrics().render();
        assert!(metrics.contains("route=\"unmatched\""));
        assert!(!metrics.contains(unique_path));
        server.abort();
    }

    #[tokio::test]
    async fn bearer_authentication_rejects_missing_and_invalid_tokens() {
        let token = "0123456789abcdef0123456789abcdef";
        let (base, server) =
            test_server_with_token(FeatureFlags::default(), Generation::Gen4, Some(token)).await;
        let client = reqwest::Client::new();

        let missing = client.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(missing.status(), reqwest::StatusCode::UNAUTHORIZED);

        let invalid = client
            .get(format!("{base}/health"))
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(invalid.status(), reqwest::StatusCode::UNAUTHORIZED);

        let accepted = client
            .get(format!("{base}/health"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(accepted.status(), reqwest::StatusCode::OK);
        server.abort();
    }

    #[tokio::test]
    async fn readiness_is_unavailable_before_backend_confirmation() {
        let (base, server) = test_server(FeatureFlags::default(), Generation::Gen4).await;
        let response = reqwest::get(format!("{base}/ready")).await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);

        let payload = response.json::<serde_json::Value>().await.unwrap();
        assert_eq!(payload["ready"], false);
        assert_eq!(payload["registered"], false);
        assert_eq!(payload["shutting_down"], false);
        server.abort();
    }

    #[tokio::test]
    async fn health_and_status_return_structured_operational_state_without_secrets() {
        let token = "0123456789abcdef0123456789abcdef";
        let features = FeatureFlags {
            gen4: true,
            rtos: true,
            ..FeatureFlags::default()
        };
        let (base, server) = test_server_with_token(features, Generation::Gen4, Some(token)).await;
        let client = reqwest::Client::new();

        let health = client
            .get(format!("{base}/health"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(health.status(), reqwest::StatusCode::OK);
        assert_eq!(
            health.json::<serde_json::Value>().await.unwrap(),
            serde_json::json!({"OK": true})
        );

        let status = client
            .get(format!("{base}/status"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(status.status(), reqwest::StatusCode::OK);
        let body = status.text().await.unwrap();
        assert!(!body.contains(token));

        let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["generation"], 4);
        assert_eq!(payload["registered"], false);
        assert_eq!(payload["shutting_down"], false);
        assert_eq!(payload["deletion_requested"], false);
        assert_eq!(payload["usb_mapping_count"], 0);
        assert_eq!(payload["gen5_mapping_count"], 0);
        assert_eq!(payload["active_capture_count"], 0);
        assert_eq!(payload["features"]["gen4"], true);
        assert_eq!(payload["features"]["rtos"], true);
        server.abort();
    }

    #[tokio::test]
    async fn metrics_endpoint_returns_prometheus_exposition() {
        let (base, server) = test_server(FeatureFlags::default(), Generation::Gen4).await;
        let response = reqwest::get(format!("{base}/metrics")).await.unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.headers()[reqwest::header::CONTENT_TYPE],
            crate::observability::metrics::PROMETHEUS_CONTENT_TYPE
        );
        assert!(response
            .text()
            .await
            .unwrap()
            .contains("# HELP edgecontroller_up"));
        server.abort();
    }

    #[tokio::test]
    async fn openapi_and_swagger_ui_are_accessible_with_correct_media_types() {
        let (base, server) = test_server(FeatureFlags::default(), Generation::Gen4).await;

        let contract = reqwest::get(format!("{base}/swagger.json")).await.unwrap();
        assert_eq!(contract.status(), reqwest::StatusCode::OK);
        assert!(contract.headers()[reqwest::header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json"));
        let document = contract.json::<serde_json::Value>().await.unwrap();
        assert_eq!(document["openapi"], "3.0.3");
        assert!(document["paths"]["/health"]["get"].is_object());

        let docs = reqwest::get(format!("{base}/docs")).await.unwrap();
        assert_eq!(docs.status(), reqwest::StatusCode::OK);
        assert!(docs.headers()[reqwest::header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html"));
        assert!(docs.text().await.unwrap().contains("url: '/swagger.json'"));
        server.abort();
    }
}
