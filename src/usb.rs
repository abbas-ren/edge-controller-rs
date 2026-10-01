//! Linux USB/TTY discovery.
//!
//! A ttyUSB number is an allocation name, not a hardware identity.
//! Resolve configured VID/PID + USB serial + interface immediately before
//! an operation.
//!
//! This detects ordinary unplug/replug and ambiguous identities. It cannot
//! make hotplug atomic with a subsequent open; do not change USB topology
//! while hardware operations are active.

use crate::error::{AppError, AppResult};

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
// Legacy helper kept to document the earlier mapping format. The runtime now
// persists the same information in the board-specific map types, but this record
// shape remains useful when comparing historical CSV layouts.
#[allow(dead_code)]
pub struct UsbMapEntry {
    pub tty: String,
    pub mac: String,
    pub serial: String,
    pub channel: u8,
}

#[derive(Debug, Clone)]
pub struct Gen5MapEntry {
    pub uart: String,
    pub power: String,
    pub mac: String,
}

/// Identity supplied by an administrator.
///
/// JSON numbers are decimal. For example:
/// FTDI VID 0x0403 = 1027, PID 0x6010 = 24592.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsbIdentity {
    pub vid: u16,
    pub pid: u16,
    pub serial: String,
    pub interface: u8,
}

impl UsbIdentity {
    pub fn validate(&self) -> AppResult<()> {
        if self.serial.is_empty()
            || self.serial.len() > 255
            || self.serial.chars().any(char::is_control)
        {
            return Err(AppError::Msg(
                "USB identity requires a nonempty, valid serial descriptor".into(),
            ));
        }

        Ok(())
    }

    pub fn matches(&self, device: &UsbTty) -> bool {
        device.vid == self.vid
            && device.pid == self.pid
            && device.serial.as_deref() == Some(self.serial.as_str())
            && device.interface == self.interface
    }
}

#[derive(Debug, Clone)]
pub struct UsbTty {
    pub tty: String,
    pub vid: u16,
    pub pid: u16,
    pub serial: Option<String>,
    pub interface: u8,

    /// Canonical sysfs USB-device location, useful for diagnostics.
    ///
    /// This is intentionally kept even when the field is not used directly in the
    /// active runtime; it helps explain how a tty node was resolved during USB
    /// discovery and makes debugging hotplug problems much easier.
    #[allow(dead_code)]
    pub topology: PathBuf,
}

fn numbered_tty(name: &str) -> bool {
    ["ttyUSB", "ttyACM"].iter().any(|prefix| {
        name.strip_prefix(prefix).is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
    })
}

fn read_hex(path: &Path) -> AppResult<u16> {
    let value = fs::read_to_string(path)?;

    u16::from_str_radix(value.trim(), 16).map_err(|_| {
        AppError::Msg(format!(
            "invalid hexadecimal sysfs value: {}",
            path.display()
        ))
    })
}

/// Inspect one canonical tty-device path.
///
/// Linux drivers place TTY nodes at different depths. Walk ancestors until
/// both the interface descriptor and owning USB device are found.
fn inspect_tty(tty: String, device_path: PathBuf) -> AppResult<Option<UsbTty>> {
    let mut interface = None;

    for ancestor in device_path.ancestors() {
        if interface.is_none() {
            let interface_path = ancestor.join("bInterfaceNumber");

            match fs::read_to_string(&interface_path) {
                Ok(value) => {
                    interface = Some(u8::from_str_radix(value.trim(), 16).map_err(|_| {
                        AppError::Msg(format!(
                            "invalid USB interface number: {}",
                            interface_path.display()
                        ))
                    })?);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }

        let vendor_path = ancestor.join("idVendor");
        if !vendor_path.exists() {
            continue;
        }

        let vid = read_hex(&vendor_path)?;
        let pid = read_hex(&ancestor.join("idProduct"))?;

        let serial = match fs::read_to_string(ancestor.join("serial")) {
            Ok(value) => {
                let value = value.trim().to_owned();
                (!value.is_empty()).then_some(value)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };

        let interface = interface
            .ok_or_else(|| AppError::Msg(format!("USB TTY {tty} has no interface descriptor")))?;

        return Ok(Some(UsbTty {
            tty,
            vid,
            pid,
            serial,
            interface,
            topology: ancestor.to_owned(),
        }));
    }

    Ok(None)
}

/// Filesystem-only scanner, separated from character-device validation so
/// tests can use a synthetic sysfs tree without root privileges.
fn scan_at(class_tty: &Path, dev_root: &Path) -> AppResult<Vec<UsbTty>> {
    let mut devices = Vec::new();

    for entry in fs::read_dir(class_tty)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };

        if !numbered_tty(name) {
            continue;
        }

        let device_path = match fs::canonicalize(entry.path().join("device")) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Device disappeared during enumeration.
                continue;
            }
            Err(error) => return Err(error.into()),
        };

        let tty = dev_root
            .join(name)
            .to_str()
            .ok_or_else(|| AppError::Msg("non-UTF-8 device pathname".into()))?
            .to_owned();

        if let Some(device) = inspect_tty(tty, device_path)? {
            devices.push(device);
        }
    }

    devices.sort_unstable_by(|left, right| left.tty.cmp(&right.tty));
    Ok(devices)
}

/// Enumerate USB serial ports whose /dev nodes currently exist.
pub fn inventory() -> AppResult<Vec<UsbTty>> {
    let scanned = scan_at(Path::new("/sys/class/tty"), Path::new("/dev"))?;
    let mut devices = Vec::with_capacity(scanned.len());

    for device in scanned {
        match fs::metadata(&device.tty) {
            Ok(metadata) if metadata.file_type().is_char_device() => {
                devices.push(device);
            }
            Ok(_) => {
                return Err(AppError::Msg(format!(
                    "{} is not a character device",
                    device.tty
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }

    Ok(devices)
}

fn select_identity(devices: &[UsbTty], identity: &UsbIdentity) -> AppResult<String> {
    identity.validate()?;

    let mut matches = devices.iter().filter(|device| identity.matches(device));

    let first = matches.next().ok_or_else(|| {
        AppError::Msg(format!(
            "USB device {:04x}:{:04x}, serial {:?}, interface {} unavailable",
            identity.vid, identity.pid, identity.serial, identity.interface
        ))
    })?;

    if matches.next().is_some() {
        return Err(AppError::Msg(
            "USB identity is ambiguous; refusing to select a device".into(),
        ));
    }

    Ok(first.tty.clone())
}

pub fn resolve_identity(identity: &UsbIdentity) -> AppResult<String> {
    select_identity(&inventory()?, identity)
}

pub fn is_relay_identity(identity: &UsbIdentity) -> bool {
    identity.vid == 0x0403 && identity.pid == 0x6001
}

fn inventory_paths(devices: &[UsbTty], vid: u16, pid: u16) -> Vec<String> {
    devices
        .iter()
        .filter(|device| device.vid == vid && device.pid == pid && device.interface == 0)
        .map(|device| device.tty.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Replace one legacy inventory file atomically.
///
/// The two inventory files are diagnostic snapshots, not transactional
/// authoritative mappings.
fn write_inventory(path: &str, lines: &[String]) -> AppResult<()> {
    let path = Path::new(path);
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Msg("inventory file has no parent directory".into()))?;

    fs::create_dir_all(parent)?;

    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o640))?;

    for line in lines {
        writeln!(temporary, "{line}")?;
    }

    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;

    Ok(())
}

/// Discover inventory only. This does not infer UART/power wiring.
pub fn discover_gen5_ttys() -> AppResult<(Vec<String>, Vec<String>)> {
    let devices = inventory()?;

    let uarts = inventory_paths(&devices, 0x0403, 0x6010);
    let powers = inventory_paths(&devices, 0x10c4, 0xea60);

    write_inventory(crate::config::GEN5_UART_TTY, &uarts)?;
    write_inventory(crate::config::GEN5_POWER_TTY, &powers)?;

    Ok((uarts, powers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn fake_tty(root: &Path, name: &str, physical: &str, interface: u8, serial: &str) {
        let usb = root.join("devices").join(physical);
        fs::create_dir_all(&usb).unwrap();
        fs::write(usb.join("idVendor"), "0403\n").unwrap();
        fs::write(usb.join("idProduct"), "6010\n").unwrap();
        fs::write(usb.join("serial"), serial).unwrap();

        let interface_path = usb.join(format!("{physical}:1.{interface}"));
        let tty_path = interface_path.join(name);
        fs::create_dir_all(&tty_path).unwrap();
        fs::write(
            interface_path.join("bInterfaceNumber"),
            format!("{interface:02x}\n"),
        )
        .unwrap();

        let class = root.join("class/tty").join(name);
        fs::create_dir_all(&class).unwrap();
        symlink(tty_path, class.join("device")).unwrap();
    }

    fn identity(serial: &str, interface: u8) -> UsbIdentity {
        UsbIdentity {
            vid: 0x0403,
            pid: 0x6010,
            serial: serial.into(),
            interface,
        }
    }

    #[test]
    fn recognizes_only_numbered_usb_and_acm_names() {
        for accepted in ["ttyUSB0", "ttyUSB12", "ttyACM0"] {
            assert!(numbered_tty(accepted));
        }

        for rejected in ["ttyUSB", "ttyUSB1x", "tty", "ttyS0", "../ttyUSB0"] {
            assert!(!numbered_tty(rejected));
        }
    }

    #[test]
    fn discovers_nested_tty_and_keeps_interfaces_separate() {
        let temp = tempfile::tempdir().unwrap();
        fake_tty(temp.path(), "ttyUSB8", "1-2", 0, "BOARD-A");
        fake_tty(temp.path(), "ttyUSB9", "1-2", 1, "BOARD-A");

        let devices = scan_at(&temp.path().join("class/tty"), Path::new("/dev")).unwrap();

        assert_eq!(
            select_identity(&devices, &identity("BOARD-A", 0)).unwrap(),
            "/dev/ttyUSB8"
        );
        assert_eq!(
            select_identity(&devices, &identity("BOARD-A", 1)).unwrap(),
            "/dev/ttyUSB9"
        );
        assert_eq!(
            inventory_paths(&devices, 0x0403, 0x6010),
            vec!["/dev/ttyUSB8"]
        );
    }

    #[test]
    fn duplicate_usb_serial_identity_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        fake_tty(temp.path(), "ttyUSB0", "1-2", 0, "DUPLICATE");
        fake_tty(temp.path(), "ttyUSB1", "1-3", 0, "DUPLICATE");

        let devices = scan_at(&temp.path().join("class/tty"), Path::new("/dev")).unwrap();

        assert!(select_identity(&devices, &identity("DUPLICATE", 0)).is_err());
    }

    #[test]
    fn absent_interface_is_not_replaced_with_another_interface() {
        let temp = tempfile::tempdir().unwrap();
        fake_tty(temp.path(), "ttyUSB4", "1-2", 1, "BOARD-A");

        let devices = scan_at(&temp.path().join("class/tty"), Path::new("/dev")).unwrap();

        assert!(select_identity(&devices, &identity("BOARD-A", 0)).is_err());
        assert!(inventory_paths(&devices, 0x0403, 0x6010).is_empty());
    }
}
