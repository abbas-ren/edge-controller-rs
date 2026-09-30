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
use std::{
    sync::{Mutex, MutexGuard},
    time::Duration,
};
use tracing::{info, warn};

const VID: u16 = 0x0403;
const PID: u16 = 0x6001;
const INTERFACE: u8 = 0;
const ENDPOINT: u8 = 0x02;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);
const TRANSFER_TIMEOUT: Duration = Duration::from_millis(500);

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

#[derive(Debug, Clone, Default)]
pub struct RelayController;

impl RelayController {
    pub fn new() -> Self {
        Self
    }

    fn validate_channel(channel: u8) -> AppResult<()> {
        if channel > 7 {
            return Err(AppError::Msg(
                "relay channel must be between 0 and 7".into(),
            ));
        }

        Ok(())
    }

    fn find(serial: &str) -> AppResult<DeviceHandle<Context>> {
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

            if descriptor.vendor_id() != VID || descriptor.product_id() != PID {
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
                Ok(found) if found == serial => return Ok(handle),
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

        // Hold the lock across discovery, configuration, read, and write.
        // Otherwise two concurrent updates could overwrite each other.
        let _guard = lock_relays()?;
        let session = Session::open(Self::find(serial)?)?;

        let current = session.read_state()?;
        let mask = 1_u8 << channel;

        let updated = if on { current | mask } else { current & !mask };

        session.write_state(updated)?;

        info!(
            %serial,
            channel,
            on,
            "relay command transferred"
        );

        // This confirms the USB transfer, not mechanical contact closure.
        Ok(())
    }

    pub fn channel_status(&self, serial: &str, channel: u8) -> AppResult<u8> {
        Self::validate_channel(channel)?;

        let _guard = lock_relays()?;
        let session = Session::open(Self::find(serial)?)?;
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
        let _guard = match lock_relays() {
            Ok(guard) => guard,
            Err(error) => {
                warn!(%error, "relay inventory unavailable");
                return Vec::new();
            }
        };

        let context = match Context::new() {
            Ok(context) => context,
            Err(error) => {
                warn!(%error, "cannot initialize USB relay inventory");
                return Vec::new();
            }
        };

        let devices = match context.devices() {
            Ok(devices) => devices,
            Err(error) => {
                warn!(%error, "cannot enumerate relay devices");
                return Vec::new();
            }
        };

        let mut inventory = Vec::new();

        for device in devices.iter() {
            let descriptor = match device.device_descriptor() {
                Ok(descriptor) => descriptor,
                Err(error) => {
                    warn!(%error, "cannot read USB descriptor");
                    continue;
                }
            };

            if descriptor.vendor_id() != VID || descriptor.product_id() != PID {
                continue;
            }

            let handle = match device.open() {
                Ok(handle) => handle,
                Err(error) => {
                    warn!(%error, "cannot open relay for inventory");
                    continue;
                }
            };

            let serial = match handle.read_serial_number_string_ascii(&descriptor) {
                Ok(serial) if !serial.is_empty() => serial,
                Ok(_) => {
                    warn!("relay has an empty serial number");
                    continue;
                }
                Err(error) => {
                    warn!(%error, "cannot read relay serial number");
                    continue;
                }
            };

            let session = match Session::open(handle) {
                Ok(session) => session,
                Err(error) => {
                    warn!(%serial, %error, "cannot claim inventory relay");
                    continue;
                }
            };

            let state = match session.read_state() {
                Ok(state) => state,
                Err(error) => {
                    warn!(%serial, %error, "cannot read inventory relay");
                    continue;
                }
            };

            let channels: serde_json::Map<String, serde_json::Value> = (0_u8..8)
                .map(|channel| {
                    (
                        format!("channel_{channel}"),
                        serde_json::json!((state >> channel) & 1),
                    )
                })
                .collect();

            inventory.push(RelayInventory {
                serial_number: serial,
                state: serde_json::Value::Object(channels),
            });
        }

        inventory.sort_unstable_by(|left, right| left.serial_number.cmp(&right.serial_number));

        inventory
    }
}

#[cfg(test)]
mod tests {
    use super::RelayController;

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
