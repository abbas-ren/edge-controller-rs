use crate::{jobs::Lease, state::RtosSession};
use axum::{
    body::Body,
    extract::{Extension, State},
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
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
use tracing::{error, warn};

use crate::{
    config::*,
    ipl,
    models::*,
    state::{normalize_mac, AppState},
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

type UsbMappings = std::collections::HashMap<(String, String, u8), String>;

type Gen5Mappings = std::collections::HashMap<String, Gen5MapEntry>;

/// Compare equal-length tokens without early exit on differing bytes.
///
/// Token length is not secret. This avoids an obvious byte-by-byte timing
/// leak without introducing an additional authentication dependency.
fn token_matches(expected: &[u8], supplied: &[u8]) -> bool {
    if expected.len() != supplied.len() {
        return false;
    }

    expected
        .iter()
        .zip(supplied)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

async fn request_guard(
    State(state): State<Arc<AppState>>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
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

    if state.jobs.is_closing() {
        return error_response(
            "controller is shutting down",
            StatusCode::SERVICE_UNAVAILABLE,
        );
    }

    let path = request.uri().path().to_owned();

    let hardware_operation = matches!(
        path.as_str(),
        "/relay"
            | "/relay/status"
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
    );

    if !hardware_operation {
        return next.run(request).await;
    }

    let lease = match state.jobs.try_enter() {
        Ok(lease) => lease,
        Err(message) => {
            return error_response(message, StatusCode::CONFLICT);
        }
    };

    // Starting/stopping captures is serialized, but multiple established
    // capture sessions are allowed.
    let capture_lifecycle = matches!(path.as_str(), "/rtos/start" | "/rtos/end");

    if !capture_lifecycle && !state.rtos_sessions.lock().await.is_empty() {
        return error_response(
            "stop and download RTOS captures before changing hardware",
            StatusCode::CONFLICT,
        );
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
    MAPPING_LOCK
        .lock()
        .map_err(|_| crate::error::AppError::Msg("mapping operation mutex was poisoned".into()))
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
        return Err(crate::error::AppError::Msg(format!(
            "{name} must be nonempty, at most 255 bytes, \
             and contain no commas or control characters"
        )));
    }

    Ok(())
}

/// Replace a mapping file atomically.
///
/// The temporary file lives in the destination directory, so rename does
/// not cross filesystem boundaries. Readers see either the old complete
/// file or the new complete file.
///
/// The file contents are synchronized before rename. This is not a full
/// power-loss durability guarantee because the parent directory is not
/// synchronized after rename.
fn atomic_mapping_write(destination: &str, contents: &[u8]) -> crate::error::AppResult<()> {
    use crate::error::AppError;
    use std::os::unix::fs::PermissionsExt;

    let destination = Path::new(destination);
    let parent = destination
        .parent()
        .ok_or_else(|| AppError::Msg("mapping file has no parent directory".into()))?;

    fs::create_dir_all(parent)?;

    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o640))?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;

    temporary
        .persist(destination)
        .map_err(|error| AppError::Io(error.error))?;

    Ok(())
}

fn persist_usb_mappings(mappings: &UsbMappings) -> crate::error::AppResult<()> {
    let mut lines = Vec::with_capacity(mappings.len());

    for ((mac, serial, channel), tty) in mappings {
        validate_csv_field("TTY", tty)?;
        validate_csv_field("relay serial", serial)?;
        let mac = validated_mac(mac)?;

        if *channel > 7 {
            return Err(crate::error::AppError::Msg(
                "stored relay channel is outside 0..7".into(),
            ));
        }

        lines.push(format!("{tty},{mac},{serial},{channel}\n"));
    }

    // Stable ordering makes configuration diffs and debugging easier.
    lines.sort_unstable();
    atomic_mapping_write(USB_MAPPING_FILE, lines.concat().as_bytes())
}

fn persist_gen5_mappings(mappings: &Gen5Mappings) -> crate::error::AppResult<()> {
    let mut lines = Vec::with_capacity(mappings.len());

    for (mac, entry) in mappings {
        validate_csv_field("UART", &entry.uart)?;
        validate_csv_field("power TTY", &entry.power)?;
        let mac = validated_mac(mac)?;

        lines.push(format!("{},{},{mac}\n", entry.uart, entry.power));
    }

    lines.sort_unstable();
    atomic_mapping_write(GEN5_MAPPING_FILE, lines.concat().as_bytes())
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
        (Ok(found), Ok(())) => Ok(found),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(probe), Err(reset)) => Err(AppError::Msg(format!(
            "UART probe failed: {probe}; board restart also failed: {reset}"
        ))),
    }
}

/// Avoid treating the FTDI relay controller itself as a board console.
///
/// An FTDI relay can initially have a ttyUSB node before its kernel driver
/// is detached for libusb access.
fn is_relay_tty(tty: &str) -> bool {
    let Some(name) = Path::new(tty).file_name() else {
        return false;
    };

    let device_path = Path::new("/sys/class/tty").join(name).join("device");

    let Ok(device_path) = fs::canonicalize(device_path) else {
        return false;
    };

    device_path.ancestors().any(|ancestor| {
        let vendor = fs::read_to_string(ancestor.join("idVendor"));
        let product = fs::read_to_string(ancestor.join("idProduct"));

        matches!(
            (vendor, product),
            (Ok(vendor), Ok(product))
                if vendor.trim().eq_ignore_ascii_case("0403")
                    && product.trim().eq_ignore_ascii_case("6001")
        )
    })
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

    let binding = state.hardware.board(&mac, generation.as_int())?;
    let relay = binding
        .relay
        .as_ref()
        .ok_or_else(|| AppError::Msg("approved relay binding missing".into()))?;

    if relay.serial != serial || relay.channel != channel {
        return Err(AppError::Msg(
            "requested relay/channel differs from approved wiring".into(),
        ));
    }

    let tty = crate::usb::resolve_identity(&binding.uart)?;

    // Do not accept an old ttyUSB pathname as a verified cache hit.
    let port = open_uart(&tty, baud)?;
    let observed = probe_with_power(port, Duration::from_secs(3), |on| {
        state.relay.set_channel(&serial, channel, on)
    })?;

    if observed.as_deref() != Some(mac.as_str()) {
        return Ok(false);
    }

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

    persist_usb_mappings(&updated)?;
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

    let mac = validated_mac(&mac)?;
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
        return Ok(None);
    }

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

    persist_gen5_mappings(&updated)?;
    *state.gen5_map.blocking_write() = updated;

    Ok(Some((uart, power)))
}

fn ok() -> Json<serde_json::Value> {
    Json(serde_json::json!({"OK": true}))
}

fn err_json(msg: &str, code: StatusCode) -> (StatusCode, Json<serde_json::Value>) {
    (code, Json(serde_json::json!({ "error": msg })))
}

fn error_response(message: impl AsRef<str>, status: StatusCode) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": message.as_ref()
        })),
    )
        .into_response()
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/confirmation", post(confirmation))
        .route("/relay", post(relay))
        .route("/relay/status", post(relay_status))
        .route("/relay/config", post(relay_config))
        .route("/devCon/delete", post(devcon_delete))
        .route("/relay/delete", post(relay_delete))
        .route("/ipl", post(ipl_run))
        .route("/ipl-mode", post(ipl_mode))
        .route("/ipl-mode/default", post(ipl_mode_default))
        .route("/gen5/tty_entry", post(gen5_tty_entry))
        .route("/gen5/power", post(gen5_power))
        .route("/mapping/entry", post(mapping_entry))
        .route("/reboot-device", post(reboot_device))
        .route("/ipl/remove", post(ipl_remove))
        .route("/rtos/start", post(rtos_start))
        .route("/rtos/end", post(rtos_end))
        .layer(middleware::from_fn_with_state(state.clone(), request_guard))
        .with_state(state)
}

fn required<T>(value: Option<T>, field: &str) -> crate::error::AppResult<T> {
    value.ok_or_else(|| crate::error::AppError::Msg(format!("{field} is required")))
}

async fn gen4_target(
    state: &AppState,
    mac: String,
    serial: String,
    channel: u8,
    gpio1: u32,
    gpio2: u32,
) -> crate::error::AppResult<ipl::Gen4Target> {
    let mac = validated_mac(&mac)?;
    validate_csv_field("serial", &serial)?;
    let approved = state.hardware.board(&mac, 4)?;
    let approved_relay = approved
        .relay
        .as_ref()
        .ok_or_else(|| crate::error::AppError::Msg("approved relay binding missing".into()))?;

    if approved_relay.serial != serial
        || approved_relay.channel != channel
        || approved.gpios != Some([gpio1, gpio2])
    {
        return Err(crate::error::AppError::Msg(
            "relay or GPIO request differs from approved board wiring".into(),
        ));
    }

    let approved_tty = crate::usb::resolve_identity(&approved.uart)?;

    if channel > 7 || gpio1 == gpio2 {
        return Err(crate::error::AppError::Msg(
            "channel must be 0..7 and GPIO offsets must differ".into(),
        ));
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

    Ok(ipl::Gen4Target {
        tty,
        mac,
        serial,
        channel,
        gpio1,
        gpio2,
    })
}

enum PreparedFlash {
    Gen4(ipl::Gen4Target, std::path::PathBuf),
    Gen5(ipl::Gen5Job),
}

async fn prepare_flash(
    state: &AppState,
    request: IplRequest,
) -> crate::error::AppResult<PreparedFlash> {
    let mac = validated_mac(&required(request.mac, "mac")?)?;
    let path = required(request.path, "path")?;

    match request.gen {
        4 => {
            let target = gen4_target(
                state,
                mac,
                required(request.serial, "serial")?,
                required(request.channel, "channel")?,
                required(request.gpio1, "gpio1")?,
                required(request.gpio2, "gpio2")?,
            )
            .await?;

            let package = task::spawn_blocking(move || {
                let package = ipl::package_directory(&path)?;
                ipl::preflight_gen4(&package)?;
                Ok::<_, crate::error::AppError>(package)
            })
            .await
            .map_err(|error| crate::error::AppError::Msg(error.to_string()))??;

            Ok(PreparedFlash::Gen4(target, package))
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
            "IPL supports gen 4 and gen 5; no Gen3 flash protocol was supplied".into(),
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
    let prepared = match prepare_flash(&state, request).await {
        Ok(prepared) => prepared,
        Err(error) => {
            return error_response(error.to_string(), StatusCode::BAD_REQUEST);
        }
    };

    let worker_state = state.clone();

    state.jobs.spawn(lease, async move {
        let result = match prepared {
            PreparedFlash::Gen4(target, package) => {
                ipl::run_gen4(worker_state, target, package).await
            }
            PreparedFlash::Gen5(job) => ipl::run_gen5(worker_state, job).await,
        };

        if let Err(error) = result {
            error!(%error, "IPL job failed");
        }
    });

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
    let target = match gen4_target(
        &state,
        request.mac,
        request.serial,
        request.channel,
        request.gpio1,
        request.gpio2,
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
    execute_ipl_mode(state, lease, request, true).await
}

async fn ipl_mode_default(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<IplModeRequest>,
) -> Response {
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
        Ok(Ok(())) => ok().into_response(),
        Ok(Err(error)) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
        Err(error) => error_response(
            format!("reboot worker failed: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

async fn confirmation(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ConfirmationRequest>,
) -> impl IntoResponse {
    match state.save_uid(&req.controller_id).await {
        Ok(_) => ok().into_response(),
        Err(e) => err_json(&e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response(),
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
                let matches = mac.as_ref().map_or(true, |value| value == stored_mac)
                    && serial.as_ref().map_or(true, |value| value == stored_serial)
                    && channel.map_or(true, |value| value == *stored_channel);

                !matches
            });

            persist_usb_mappings(&updated)?;
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

            persist_gen5_mappings(&updated)?;
            *state.gen5_map.blocking_write() = updated;
        }
    }

    Ok(())
}

async fn devcon_delete(
    State(state): State<Arc<AppState>>,
    Json(request): Json<DeleteRequest>,
) -> Response {
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

        // Means "stop this service", not "reboot the Raspberry Pi".
        *state.reboot.write().await = true;
    }

    ok().into_response()
}

async fn relay_delete(
    State(state): State<Arc<AppState>>,
    Json(request): Json<DeleteRequest>,
) -> Response {
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
        Ok(Ok(())) => ok().into_response(),
        Ok(Err(error)) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
        Err(error) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn gen5_tty_entry(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Gen5TtyRequest>,
) -> impl IntoResponse {
    match resolve_gen5_mapping_sync(state.clone(), &req.mac) {
        Ok(Some((uart, power))) => {
            let body = serde_json::json!({ "uart": uart, "power": power });
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

    if !state
        .hardware
        .relay_allowed(&request.serial, request.channel)
    {
        return error_response(
            "relay/channel is not approved by hardware policy",
            StatusCode::FORBIDDEN,
        );
    }

    let result = task::spawn_blocking(move || {
        let _lease = lease;
        state
            .relay
            .set_channel(&request.serial, request.channel, on)
    })
    .await;

    match result {
        Ok(Ok(())) => ok().into_response(),
        Ok(Err(error)) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
        Err(error) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn relay_status(
    State(state): State<Arc<AppState>>,
    Extension(lease): Extension<Lease>,
    Json(request): Json<RelayStatusRequest>,
) -> Response {
    if request.channel > 7 {
        return error_response("relay channel must be 0..7", StatusCode::BAD_REQUEST);
    }

    if let Err(error) = validate_csv_field("serial", &request.serial) {
        return error_response(error.to_string(), StatusCode::BAD_REQUEST);
    }

    if !state
        .hardware
        .relay_allowed(&request.serial, request.channel)
    {
        return error_response(
            "relay/channel is not approved by hardware policy",
            StatusCode::FORBIDDEN,
        );
    }

    let result = task::spawn_blocking(move || {
        let _lease = lease;
        state.relay.channel_status(&request.serial, request.channel)
    })
    .await;

    match result {
        Ok(Ok(value)) => Json(serde_json::json!({ "state": value })).into_response(),
        Ok(Err(error)) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
        Err(error) => error_response(
            format!("relay status worker failed: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

async fn relay_config(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RelayConfigRequest>,
) -> Response {
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
        Ok(Ok(())) => ok().into_response(),
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
        Ok(Ok((uart, power))) => Json(serde_json::json!({
            "uart": uart,
            "power": power,
        }))
        .into_response(),
        Ok(Err(error)) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
        Err(error) => error_response(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn ipl_remove(
    State(_state): State<Arc<AppState>>,
    Json(request): Json<RemoveIplRequest>,
) -> Response {
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
    let normalized = normalize_mac(value);

    if normalized.len() != 12 || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(crate::error::AppError::Msg(
            "MAC address must contain exactly 12 hexadecimal digits".into(),
        ));
    }

    Ok(normalized)
}

/// Resolve symlinks and restrict serial access to actual USB/ACM character
/// devices. This also permits /dev/serial/by-id/... aliases.
///
/// This is a device-type check, not authorization. A production deployment
/// should additionally allowlist the expected device identities.
fn checked_tty(value: &str) -> crate::error::AppResult<String> {
    use std::os::unix::fs::FileTypeExt;

    let path = fs::canonicalize(value)?;
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
        return Err(crate::error::AppError::Msg(
            "expected a USB or ACM serial character device".into(),
        ));
    }

    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| crate::error::AppError::Msg("non-UTF-8 TTY path".into()))
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

        if serial != Some(relay.serial.as_str()) || channel != Some(relay.channel) {
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

    sessions.insert(
        session_key,
        RtosSession {
            tty,
            file,
            stop,
            handle: Some(handle),
        },
    );

    ok().into_response()
}

async fn rtos_end(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RtosEndRequest>,
) -> Response {
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

    state
        .client
        .post(url)
        .json(&serde_json::json!({
            "mac": mac,
            "status": if success { "success" } else { "failure" },
        }))
        .send()
        .await?
        .error_for_status()?;

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

    state
        .client
        .post(url)
        .json(&payload)
        .send()
        .await?
        .error_for_status()?;

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
        Ok(true)
    } else if value.eq_ignore_ascii_case("off") {
        Ok(false)
    } else {
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
        return Err(crate::error::AppError::Msg(
            "power device is not present in a stored Gen5 mapping".into(),
        ));
    }

    Ok(supplied)
}

#[cfg(test)]
mod tests {
    use super::{extract_uboot_mac, validated_mac};

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
}
