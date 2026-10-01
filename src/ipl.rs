//! Gen3/Gen4 FlashWriter protocol and a Gen5 external-script adapter.
//!
//! Hardware sequencing runs in blocking workers. Callers must retain an
//! exclusive hardware-operation lease until these functions finish.

use crate::{
    error::{AppError, AppResult},
    gpio::GpioController,
    observability::metrics::global_metrics,
    state::AppState,
    uart::Console,
};

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use tracing::{error, info};

pub const FIRMWARE_ROOT: &str = "/var/lib/dev-controller/firmware";
const LOG_ROOT: &str = "/var/lib/dev-controller/ipl";

const FLASHWRITER: &str = "ICUMX_Flash_writer_SCIF_DUMMY_CERT_EB203000_V4H.mot";

const MAX_IMAGE_BYTES: u64 = 128 * 1024 * 1024;

pub fn supports_relay_flash(generation: u8) -> bool {
    matches!(generation, 3 | 4)
}

struct Image {
    name: &'static str,
    save: &'static str,
    address: &'static str,
    emmc: bool,
}

const IMAGES: [Image; 8] = [
    Image {
        name: "bootparam_sa0.srec",
        save: "0",
        address: "EB200000",
        emmc: false,
    },
    Image {
        name: "icumx_loader.srec",
        save: "40000",
        address: "EB210000",
        emmc: false,
    },
    Image {
        name: "cert_header_sa9.srec",
        save: "240000",
        address: "EB230000",
        emmc: false,
    },
    Image {
        name: "dummy_fw.srec",
        save: "280000",
        address: "EB240000",
        emmc: false,
    },
    Image {
        name: "cr52_loader.srec",
        save: "480000",
        address: "E6300000",
        emmc: false,
    },
    Image {
        name: "dummy_rtos.srec",
        save: "2800",
        address: "E2100000",
        emmc: true,
    },
    Image {
        name: "bl31-whitehawk.srec",
        save: "a000",
        address: "46400000",
        emmc: true,
    },
    Image {
        name: "u-boot-elf-whitehawk.srec",
        save: "ac00",
        address: "50000000",
        emmc: true,
    },
];

#[derive(Clone)]
pub struct FlashWriterTarget {
    pub generation: u8,
    pub tty: String,
    pub mac: String,
    pub serial: String,
    pub channel: u8,
    pub gpio1: u32,
    pub gpio2: u32,
}

pub struct Gen5Job {
    pub package: PathBuf,
    pub script: PathBuf,
    pub sdk_version: String,
    pub uart: String,
    pub power: String,
    pub mac: String,
}

/// Firmware storage must be administrator-controlled.
///
/// Canonical paths reject symlinks escaping the storage root. Preventing
/// hostile local rename races additionally requires trusted ownership of
/// the directory tree.
pub fn package_directory(value: &str) -> AppResult<PathBuf> {
    let root = fs::canonicalize(FIRMWARE_ROOT)?;
    let package = fs::canonicalize(value)?;

    if package == root || !package.starts_with(&root) || !package.is_dir() {
        return Err(AppError::Msg(
            "firmware package must be a directory below the firmware root".into(),
        ));
    }

    Ok(package)
}

fn image_file(package: &Path, name: &str) -> AppResult<PathBuf> {
    let path = fs::canonicalize(package.join(name))?;
    let metadata = fs::metadata(&path)?;

    if !path.starts_with(package)
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_IMAGE_BYTES
    {
        return Err(AppError::Msg(format!(
            "invalid or oversized firmware image: {name}"
        )));
    }

    Ok(path)
}

pub fn preflight_flashwriter(package: &Path) -> AppResult<()> {
    image_file(package, FLASHWRITER)?;

    for image in &IMAGES {
        image_file(package, image.name)?;
    }

    Ok(())
}

pub fn gen5_script() -> AppResult<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let path = PathBuf::from(
        std::env::var("DEV_CONTROLLER_GEN5_SCRIPT")
            .unwrap_or_else(|_| "/usr/local/libexec/dev-controller/gen5_ipl.sh".into()),
    );

    if !path.is_absolute() {
        return Err(AppError::Msg(
            "Gen5 script must have an absolute pathname".into(),
        ));
    }

    let path = fs::canonicalize(path)?;
    let metadata = fs::metadata(&path)?;

    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(AppError::Msg(
            "Gen5 flashing script is not an executable regular file".into(),
        ));
    }

    Ok(path)
}

fn seconds(value: u64) -> Duration {
    Duration::from_secs(value)
}

/// Change boot straps while power is off.
///
/// This deliberately leaves the GPIO line request owned by the service.
fn set_boot_mode(state: &AppState, target: &FlashWriterTarget, download: bool) -> AppResult<()> {
    state
        .relay
        .set_channel(&target.serial, target.channel, false)?;

    std::thread::sleep(seconds(2));

    GpioController::set_pair(target.gpio1, target.gpio2, download)?;

    std::thread::sleep(seconds(2));

    state
        .relay
        .set_channel(&target.serial, target.channel, true)
}

/// Enter SCIF download mode, opening UART before the boot begins.
fn download_console(state: &AppState, target: &FlashWriterTarget) -> AppResult<Console> {
    let mut console = Console::open(&target.tty, 921_600)?;
    console.clear_input()?;

    set_boot_mode(state, target, true)?;

    console.wait_any(&[b"please send !", b"please send", b"send !"], seconds(50))?;

    Ok(console)
}

fn combine(operation: AppResult<()>, cleanup: AppResult<()>) -> AppResult<()> {
    match (operation, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(operation), Err(cleanup)) => Err(AppError::Msg(format!(
            "{operation}; restoring default boot mode also failed: {cleanup}"
        ))),
    }
}

/// Used by /ipl-mode and /ipl-mode/default.
///
/// On download-mode failure, attempt to restore normal boot. Unlike the
/// legacy default-mode handler, normal boot does not wait for a download
/// prompt that should not appear.
pub fn mode_sync(state: &AppState, target: &FlashWriterTarget, download: bool) -> AppResult<()> {
    if !download {
        return set_boot_mode(state, target, false);
    }

    match download_console(state, target) {
        Ok(console) => {
            drop(console);
            Ok(())
        }
        Err(error) => combine(Err(error), set_boot_mode(state, target, false)),
    }
}

fn transfer_image(console: &mut Console, package: &Path, image: &Image) -> AppResult<()> {
    let file = image_file(package, image.name)?;

    if image.emmc {
        console.command("em_w\r\n")?;
        console.wait(b"(Push Y key)", seconds(10))?;
        console.send(b"y")?;

        console.wait(b"Select area(0-2)>", seconds(10))?;
        console.command("1\r\n")?;

        console.wait(b"Please Input Start Address in sector :", seconds(10))?;
        console.command(&format!("{}\r\n", image.save))?;

        console.wait(b"Please Input Program Start Address :", seconds(10))?;
        console.command(&format!("{}\r\n", image.address))?;
    } else {
        console.command("xls2\r\n")?;
        console.wait(b"Select (1-3)>", seconds(10))?;
        console.command("1\r\n")?;

        console.wait(b"(Push Y key)", seconds(10))?;
        console.send(b"y")?;

        console.wait(b"(Push Y key)", seconds(10))?;
        console.send(b"y")?;

        console.wait(b"Please Input : H", seconds(10))?;
        console.command(&format!("{}\r\n", image.address))?;

        console.wait(b"Please Input : H", seconds(10))?;
        console.command(&format!("{}\r\n", image.save))?;
    }

    console.wait(b"please send ! (Motorola S-record)", seconds(15))?;
    std::thread::sleep(Duration::from_millis(400));

    console.send_srec(&file)?;

    if image.emmc {
        console.wait(b"EM_W Complete!", seconds(60))?;
    } else {
        let response = console.wait_any(&[b"complete!", b"Clear OK?(y/n)"], seconds(60))?;

        if response == 1 {
            console.send(b"y")?;
            console.wait(b">", seconds(10))?;
        }
    }

    Ok(())
}

fn log_file(generation: u8, mac: &str) -> AppResult<(PathBuf, File)> {
    // The MAC used in a filename must already be normalized and validated.
    if mac.len() != 12 || !mac.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::Msg("invalid log identity".into()));
    }

    fs::create_dir_all(LOG_ROOT)?;
    let path = Path::new(LOG_ROOT).join(format!("gen{generation}-{mac}.log"));

    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)?;

    Ok((path, file))
}

fn flashwriter_sync(state: &AppState, target: &FlashWriterTarget, package: &Path) -> AppResult<()> {
    preflight_flashwriter(package)?;
    let (_, mut log) = log_file(target.generation, &target.mac)?;

    writeln!(
        log,
        "Starting Gen{} IPL for {}",
        target.generation, target.mac
    )?;

    let operation = (|| {
        let mut console = download_console(state, target)?;

        console.send_srec(&image_file(package, FLASHWRITER)?)?;
        console.wait(b">", seconds(30))?;

        for image in &IMAGES {
            writeln!(log, "Programming {}", image.name)?;
            log.flush()?;

            transfer_image(&mut console, package, image)?;
            std::thread::sleep(seconds(1));
        }

        // Drop UART ownership before restarting the board.
        drop(console);
        Ok(())
    })();

    let result = combine(operation, set_boot_mode(state, target, false));

    match &result {
        Ok(()) => writeln!(log, "IPL completed successfully")?,
        Err(error) => writeln!(log, "IPL failed: {error}")?,
    }

    log.flush()?;
    result
}

fn completion_endpoint(generation: u8) -> &'static str {
    if supports_relay_flash(generation) {
        "flash-confirm-gen4"
    } else {
        "flash-confirm"
    }
}

async fn notify(state: &AppState, generation: u8, success: bool) {
    let endpoint = completion_endpoint(generation);

    let status = if success { "success" } else { "failure" };

    let url = format!(
        "http://{}:{}/api/v1/device/{endpoint}?status={status}",
        state.cfg.server_ip, state.cfg.http_port,
    );

    let result = match state.client.get(url).send().await {
        Ok(response) => response.error_for_status(),
        Err(error) => Err(error),
    };

    if let Err(error) = result {
        error!(
            %error,
            generation,
            success,
            "IPL completion notification failed"
        );
    }
}

pub async fn run_flashwriter(
    state: Arc<AppState>,
    target: FlashWriterTarget,
    package: PathBuf,
) -> AppResult<()> {
    let generation = target.generation;
    let started = std::time::Instant::now();
    let worker_state = state.clone();

    let result = match tokio::task::spawn_blocking(move || {
        flashwriter_sync(&worker_state, &target, &package)
    })
    .await
    {
        Ok(result) => result,
        Err(error) => Err(AppError::Msg(format!(
            "Gen{generation} flash worker failed: {error}"
        ))),
    };

    global_metrics().record_firmware_flash(
        generation,
        result.is_ok(),
        started.elapsed().as_secs_f64(),
    );
    notify(&state, generation, result.is_ok()).await;
    result
}

/// Search a log with bounded memory, including matches across read boundaries.
fn contains_success_marker(path: &Path) -> AppResult<bool> {
    const MARKER: &[u8] = b"Flash process completed successfully";

    let mut file = File::open(path)?;
    let mut pending = Vec::with_capacity(8192);
    let mut buffer = [0_u8; 4096];

    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(false);
        }

        pending.extend_from_slice(&buffer[..count]);

        if pending.windows(MARKER.len()).any(|part| part == MARKER) {
            return Ok(true);
        }

        let keep = MARKER.len() - 1;
        if pending.len() > keep {
            pending.drain(..pending.len() - keep);
        }
    }
}

/// Run the administrator-installed Gen5 script without shell interpolation.
///
/// Arguments preserve the legacy invocation:
/// sdk_version, package/HIL, power_tty, uart_tty.
///
/// Success requires BOTH a zero exit status and the legacy success marker.
/// stdout/stderr stream to disk rather than accumulating in memory.
pub async fn run_gen5(state: Arc<AppState>, job: Gen5Job) -> AppResult<()> {
    let started = std::time::Instant::now();
    let result: AppResult<()> = async {
        let hil = fs::canonicalize(job.package.join("HIL"))?;

        if !hil.starts_with(&job.package) || !hil.is_dir() {
            return Err(AppError::Msg("invalid Gen5 HIL directory".into()));
        }

        let (log_path, log) = log_file(5, &job.mac)?;
        let stderr = log.try_clone()?;

        let mut child = tokio::process::Command::new(&job.script)
            .arg(&job.sdk_version)
            .arg(&hil)
            .arg(&job.power)
            .arg(&job.uart)
            .current_dir(&job.package)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr))
            .spawn()?;

        // Deliberately do not abort a firmware-writing process on a generic
        // request timeout. Its safe cancellation protocol is unknown.
        let status = child.wait().await?;

        if !status.success() {
            return Err(AppError::Msg(format!("Gen5 script exited with {status}")));
        }

        let marker = tokio::task::spawn_blocking(move || contains_success_marker(&log_path))
            .await
            .map_err(|error| AppError::Msg(error.to_string()))??;

        if !marker {
            return Err(AppError::Msg(
                "Gen5 script exited without its success marker".into(),
            ));
        }

        info!("Gen5 IPL completed");
        Ok(())
    }
    .await;

    global_metrics().record_firmware_flash(5, result.is_ok(), started.elapsed().as_secs_f64());
    notify(&state, 5, result.is_ok()).await;
    result
}

#[cfg(test)]
mod tests {
    use super::{
        completion_endpoint, contains_success_marker, preflight_flashwriter, supports_relay_flash,
        FLASHWRITER, IMAGES,
    };
    use std::{fs, os::unix::fs::symlink};

    fn complete_package() -> tempfile::TempDir {
        let package = tempfile::tempdir().unwrap();
        fs::write(package.path().join(FLASHWRITER), b"flashwriter").unwrap();
        for image in &IMAGES {
            fs::write(package.path().join(image.name), b"image").unwrap();
        }
        package
    }

    #[test]
    fn gen3_and_gen4_share_the_relay_flash_protocol() {
        assert!(supports_relay_flash(3));
        assert!(supports_relay_flash(4));
        assert!(!supports_relay_flash(5));
        assert_eq!(completion_endpoint(3), "flash-confirm-gen4");
        assert_eq!(completion_endpoint(4), "flash-confirm-gen4");
        assert_eq!(completion_endpoint(5), "flash-confirm");
    }

    #[test]
    fn preflight_accepts_a_complete_nonempty_package() {
        let package = complete_package();
        assert!(preflight_flashwriter(package.path()).is_ok());
    }

    #[test]
    fn preflight_rejects_missing_and_empty_images() {
        let package = complete_package();
        fs::remove_file(package.path().join(IMAGES[0].name)).unwrap();
        assert!(preflight_flashwriter(package.path()).is_err());

        fs::write(package.path().join(IMAGES[0].name), []).unwrap();
        assert!(preflight_flashwriter(package.path()).is_err());
    }

    #[test]
    fn preflight_rejects_an_image_symlink_outside_the_package() {
        let package = complete_package();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let image = package.path().join(IMAGES[0].name);
        fs::remove_file(&image).unwrap();
        symlink(outside.path(), image).unwrap();

        assert!(preflight_flashwriter(package.path()).is_err());
    }

    #[test]
    fn success_marker_is_detected_across_read_boundaries() {
        let log = tempfile::NamedTempFile::new().unwrap();
        let mut contents = vec![b'x'; 4090];
        contents.extend_from_slice(b"Flash process completed successfully\n");
        fs::write(log.path(), contents).unwrap();
        assert!(contains_success_marker(log.path()).unwrap());

        fs::write(log.path(), b"Flash process failed").unwrap();
        assert!(!contains_success_marker(log.path()).unwrap());
    }
}
