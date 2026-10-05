//! FTDI bit-bang relay control.
//!
//! Channel numbering is zero-based: 0..7.
//!
//! The USB interface is claimed for each operation and released through
//! RAII. The kernel serial driver is detached when necessary and is not
//! automatically reattached: this matches the legacy bit-bang usage.
//!
//! Only one process should own these relay devices. The mutex below
//! serializes this process, not other applications.

use crate::{
    error::{AppError, AppResult},
    models::RelayInventory,
};

use rusb::{Context, DeviceHandle, UsbContext};
use serde::Serialize;
use std::{
    sync::{Arc, Mutex, MutexGuard, RwLock},
    time::Duration,
};
use tracing::{info, warn};

const INTERFACE: u8 = 0;
const ENDPOINT: u8 = 0x02;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(1);

/// Protect an entire read-modify-write transaction, not just the USB write.
static RELAY_LOCK: Mutex<()> = Mutex::new(());

fn lock_relays() -> AppResult<MutexGuard<'static, ()>> {
    RELAY_LOCK
        .lock()
        .map_err(|_| AppError::Msg("relay mutex was poisoned".into()))
}

fn usb_error(operation: &str, error: rusb::Error) -> AppError {
    AppError::Msg(format!("{operation}: {error}"))
}

/// A successfully claimed interface.
///
/// If configuring bit-bang mode subsequently fails, Drop still releases
/// the interface.
struct Session {
    handle: DeviceHandle<Context>,
}

impl Session {
    fn open(handle: DeviceHandle<Context>) -> AppResult<Self> {
        match handle.kernel_driver_active(INTERFACE) {
            Ok(true) => {
                handle
                    .detach_kernel_driver(INTERFACE)
                    .map_err(|error| usb_error("detaching relay serial driver", error))?;
            }
            Ok(false) => {}
            Err(error) => {
                return Err(usb_error("checking relay kernel driver", error));
            }
        }

        handle
            .claim_interface(INTERFACE)
            .map_err(|error| usb_error("claiming relay interface", error))?;

        let session = Self { handle };

        // FTDI SET_BITMODE: all eight pins are outputs, bit-bang mode 1.
        let written = session
            .handle
            .write_control(0x40, 0x0B, 0x01FF, 0x01, &[], CONTROL_TIMEOUT)
            .map_err(|error| usb_error("enabling relay bit-bang mode", error))?;

        if written != 0 {
            return Err(AppError::Msg(
                "unexpected bit-bang control-transfer length".into(),
            ));
        }

        Ok(session)
    }

    fn read_state(&self) -> AppResult<u8> {
        let mut response = [0_u8; 2];

        let count = self
            .handle
            .read_control(0xC0, 0x0C, 0, 0x01, &mut response, TRANSFER_TIMEOUT)
            .map_err(|error| usb_error("reading relay state", error))?;

        if count == 0 {
            return Err(AppError::Msg(
                "relay returned an empty state response".into(),
            ));
        }

        Ok(response[0])
    }

    fn write_state(&self, state: u8) -> AppResult<()> {
        let count = self
            .handle
            .write_bulk(ENDPOINT, &[state], TRANSFER_TIMEOUT)
            .map_err(|error| usb_error("writing relay state", error))?;

        if count != 1 {
            return Err(AppError::Msg(format!(
                "short relay write: expected 1 byte, transferred {count}"
            )));
        }

        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Err(error) = self.handle.release_interface(INTERFACE) {
            warn!(%error, "could not release relay interface");
        }
    }
}

/// User-supplied selector for the active relay board.
#[derive(Debug, Clone)]
pub struct RelaySelector {
    /// Optional USB iSerial value.
    pub serial_number: Option<String>,
    /// Optional hexadecimal `VID:PID` selector.
    pub vid_pid: Option<String>,
}

/// Resolved identity of the relay board used by hardware operations.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayIdentity {
    /// USB iSerial value.
    pub serial_number: String,
    /// USB vendor identifier.
    pub vid: u16,
    /// USB product identifier.
    pub pid: u16,
}

/// FTDI relay controller backed by a runtime-updateable USB identity.
#[derive(Debug, Clone)]
pub struct RelayController {
    identity: Arc<RwLock<Option<RelayIdentity>>>,
}

impl RelayController {
    /// Resolve an optional startup selector and construct the controller.
    pub fn new(selector: Option<RelaySelector>) -> AppResult<Self> {
        let identity = selector.map(Self::resolve).transpose()?;
        Ok(Self {
            identity: Arc::new(RwLock::new(identity)),
        })
    }

    fn parse_vid_pid(value: &str) -> AppResult<(u16, u16)> {
        let (vid, pid) = value
            .split_once(':')
            .ok_or_else(|| AppError::Msg("VID:PID must contain one ':' separator".into()))?;
        if vid.len() != 4 || pid.len() != 4 {
            return Err(AppError::Msg(
                "VID:PID must use four hexadecimal digits per component".into(),
            ));
        }
        let vid = u16::from_str_radix(vid, 16)
            .map_err(|_| AppError::Msg("VID contains non-hexadecimal characters".into()))?;
        let pid = u16::from_str_radix(pid, 16)
            .map_err(|_| AppError::Msg("PID contains non-hexadecimal characters".into()))?;
        Ok((vid, pid))
    }

    /// Validate VID:PID and use an optional serial, discovering iSerial when absent.
    pub fn resolve(selector: RelaySelector) -> AppResult<RelayIdentity> {
        let vid_pid = selector
            .vid_pid
            .as_deref()
            .ok_or_else(|| AppError::Msg("VID:PID is required for relay selection".into()))?;
        let (vid, pid) = Self::parse_vid_pid(vid_pid)?;

        if let Some(serial_number) = selector.serial_number {
            crate::store::validate_serial(&serial_number)?;
            return Ok(RelayIdentity {
                serial_number,
                vid,
                pid,
            });
        }

        Self::discover_serial(vid, pid)
    }

    fn discover_serial(vid: u16, pid: u16) -> AppResult<RelayIdentity> {
        let context = Context::new().map_err(|error| usb_error("initializing libusb", error))?;
        let devices = context
            .devices()
            .map_err(|error| usb_error("enumerating USB devices", error))?;
        let mut found = None;

        for device in devices.iter() {
            let descriptor = match device.device_descriptor() {
                Ok(descriptor)
                    if descriptor.vendor_id() == vid && descriptor.product_id() == pid =>
                {
                    descriptor
                }
                Ok(_) => continue,
                Err(error) => {
                    warn!(%error, "could not read USB descriptor");
                    continue;
                }
            };
            let handle = device
                .open()
                .map_err(|error| usb_error("opening relay selected by VID:PID", error))?;
            let serial_number = handle
                .read_serial_number_string_ascii(&descriptor)
                .map_err(|error| usb_error("reading relay iSerial", error))?;
            crate::store::validate_serial(&serial_number)?;
            if found.is_some() {
                return Err(AppError::Msg(format!(
                    "multiple USB devices match {vid:04x}:{pid:04x}; supply a serial number"
                )));
            }
            found = Some(RelayIdentity {
                serial_number,
                vid,
                pid,
            });
        }

        found.ok_or_else(|| AppError::Msg(format!("relay {vid:04x}:{pid:04x} not found")))
    }

    /// Return the currently active relay identity.
    pub fn identity(&self) -> AppResult<RelayIdentity> {
        self.identity
            .read()
            .map_err(|_| AppError::Msg("relay identity lock was poisoned".into()))?
            .clone()
            .ok_or_else(|| AppError::Msg("relay identity is not configured".into()))
    }

    /// Replace the active relay identity without restarting the service.
    pub fn update_identity(&self, identity: RelayIdentity) -> AppResult<()> {
        *self
            .identity
            .write()
            .map_err(|_| AppError::Msg("relay identity lock was poisoned".into()))? =
            Some(identity);
        Ok(())
    }

    fn validate_channel(channel: u8) -> AppResult<()> {
        if channel > 7 {
            return Err(AppError::Msg(
                "relay channel must be between 0 and 7".into(),
            ));
        }

        Ok(())
    }

    fn find(identity: &RelayIdentity) -> AppResult<DeviceHandle<Context>> {
        let serial = &identity.serial_number;
        if serial.is_empty() || serial.chars().any(char::is_control) {
            return Err(AppError::Msg("invalid relay serial number".into()));
        }

        let context = Context::new().map_err(|error| usb_error("initializing libusb", error))?;

        let devices = context
            .devices()
            .map_err(|error| usb_error("enumerating USB devices", error))?;

        for device in devices.iter() {
            let descriptor = match device.device_descriptor() {
                Ok(descriptor) => descriptor,
                Err(error) => {
                    warn!(%error, "could not read USB descriptor");
                    continue;
                }
            };

            if descriptor.vendor_id() != identity.vid || descriptor.product_id() != identity.pid {
                continue;
            }

            let handle = match device.open() {
                Ok(handle) => handle,
                Err(error) => {
                    warn!(%error, "could not open candidate relay");
                    continue;
                }
            };

            match handle.read_serial_number_string_ascii(&descriptor) {
                Ok(found) if found == serial.as_str() => return Ok(handle),
                Ok(_) => {}
                Err(error) => {
                    warn!(%error, "could not read candidate relay serial");
                }
            }
        }

        Err(AppError::Msg(format!(
            "relay {serial:?} not found or inaccessible"
        )))
    }

    pub fn set_channel(&self, serial: &str, channel: u8, on: bool) -> AppResult<()> {
        Self::validate_channel(channel)?;
        let identity = self.identity()?;
        if serial != identity.serial_number {
            return Err(AppError::Msg(format!(
                "relay serial {serial:?} is not the active relay"
            )));
        }

        tracing::debug!(%serial, channel, on, "attempting relay channel update");

        // Hold the lock across discovery, configuration, read, and write.
        // Otherwise two concurrent updates could overwrite each other.
        let _guard = lock_relays()?;
        let session = Session::open(Self::find(&identity)?)?;

        let current = session.read_state()?;
        let mask = 1_u8 << channel;

        let updated = if on { current | mask } else { current & !mask };

        if updated == current {
            tracing::debug!(state = current, "relay channel already has requested state");
            return Ok(());
        }

        session.write_state(updated)?;

        info!(
            %serial,
            channel,
            on,
            "relay command transferred"
        );
        tracing::debug!(
            previous_state = current,
            updated_state = updated,
            "relay bitmask updated"
        );

        // This confirms the USB transfer, not mechanical contact closure.
        Ok(())
    }

    pub fn channel_status(&self, serial: &str, channel: u8) -> AppResult<u8> {
        Self::validate_channel(channel)?;
        let identity = self.identity()?;
        if serial != identity.serial_number {
            return Err(AppError::Msg(format!(
                "relay serial {serial:?} is not the active relay"
            )));
        }

        let _guard = lock_relays()?;
        let session = Session::open(Self::find(&identity)?)?;
        let state = session.read_state()?;

        Ok((state >> channel) & 1)
    }

    /// Collect readable relay states without issuing channel-state writes.
    ///
    /// Configuring FTDI bit-bang mode still changes the USB device's mode;
    /// inventory must therefore run before accepting hardware requests.
    ///
    /// Device failures are logged and omitted from the inventory. An empty
    /// result means "no readable relays", not necessarily "none attached".
    pub fn inventory(&self) -> Vec<RelayInventory> {
        tracing::debug!("enumerating relay inventory");

        let _guard = match lock_relays() {
            Ok(guard) => guard,
            Err(error) => {
                warn!(%error, "relay inventory unavailable");
                return Vec::new();
            }
        };

        let identity = match self.identity() {
            Ok(identity) => identity,
            Err(error) => {
                warn!(%error, "relay inventory unavailable");
                return Vec::new();
            }
        };
        let serial = identity.serial_number.clone();
        let session = match Self::find(&identity).and_then(Session::open) {
            Ok(session) => session,
            Err(error) => {
                warn!(%serial, %error, "cannot claim inventory relay");
                return Vec::new();
            }
        };
        let state = match session.read_state() {
            Ok(state) => state,
            Err(error) => {
                warn!(%serial, %error, "cannot read inventory relay");
                return Vec::new();
            }
        };
        let channels = (0_u8..8)
            .map(|channel| {
                (
                    format!("channel_{channel}"),
                    serde_json::json!((state >> channel) & 1),
                )
            })
            .collect();

        tracing::info!(relay_count = 1, "relay inventory discovered");
        vec![RelayInventory {
            serial_number: serial,
            state: serde_json::Value::Object(channels),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::{RelayController, RelaySelector};

    #[test]
    fn relay_serial_requires_vid_pid() {
        let result = RelayController::resolve(RelaySelector {
            serial_number: Some("RELAY-A".to_owned()),
            vid_pid: None,
        });

        assert!(result
            .unwrap_err()
            .to_string()
            .contains("VID:PID is required"));
    }

    #[test]
    fn relay_serial_and_vid_pid_resolve_without_usb_discovery() {
        let identity = RelayController::resolve(RelaySelector {
            serial_number: Some("RELAY-A".to_owned()),
            vid_pid: Some("0403:6001".to_owned()),
        })
        .unwrap();

        assert_eq!(identity.serial_number, "RELAY-A");
        assert_eq!((identity.vid, identity.pid), (0x0403, 0x6001));
    }

    #[test]
    fn valid_channels_are_zero_through_seven() {
        for channel in 0..8 {
            assert!(RelayController::validate_channel(channel).is_ok());
        }
    }

    #[test]
    fn invalid_channels_are_rejected_before_usb_access() {
        for channel in [8, 16, 255] {
            assert!(RelayController::validate_channel(channel).is_err());
        }
    }
}
