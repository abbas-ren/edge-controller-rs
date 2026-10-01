//! Administrator-approved board wiring.
//!
//! The policy is loaded once at startup. Editing it requires a service
//! restart. Never accept replacement bindings through an untrusted API.

use crate::{
    error::{AppError, AppResult},
    usb::{self, UsbIdentity},
};

use serde::Deserialize;
use std::collections::HashSet;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayBinding {
    pub serial: String,
    pub channel: u8,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardBinding {
    pub mac: String,
    pub gen: u8,
    pub uart: UsbIdentity,
    pub power: Option<UsbIdentity>,
    pub relay: Option<RelayBinding>,
    pub gpios: Option<[u32; 2]>,
    pub rtos: Option<UsbIdentity>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HardwarePolicy {
    pub boards: Vec<BoardBinding>,
}

impl HardwarePolicy {
    pub fn load(path: &str) -> AppResult<Self> {
        let text = crate::store::read_required_text(std::path::Path::new(path), 64 * 1024)?;

        let mut policy: Self = serde_json::from_str(&text)
            .map_err(|error| AppError::Msg(format!("invalid hardware policy {path}: {error}")))?;

        policy.validate()?;
        Ok(policy)
    }

    fn validate(&mut self) -> AppResult<()> {
        let mut macs = HashSet::new();
        let mut identities = HashSet::new();
        let mut relay_channels = HashSet::new();
        let mut gpios = HashSet::new();

        tracing::info!(board_count = self.boards.len(), "validating hardware policy bindings");

        for board in &mut self.boards {
            board.mac = crate::store::checked_mac(&board.mac)?;

            if board.mac.len() != 12
                || !board.mac.bytes().all(|byte| byte.is_ascii_hexdigit())
                || !macs.insert(board.mac.clone())
            {
                return Err(AppError::Msg(
                    "hardware policy has an invalid or duplicate MAC".into(),
                ));
            }

            if !matches!(board.gen, 3..=5) {
                return Err(AppError::Msg("invalid board generation".into()));
            }

            for identity in [Some(&board.uart), board.power.as_ref(), board.rtos.as_ref()]
                .into_iter()
                .flatten()
            {
                identity.validate()?;

                if usb::is_relay_identity(identity) {
                    return Err(AppError::Msg(
                        "an FTDI relay cannot be assigned as a serial console".into(),
                    ));
                }

                if !identities.insert(identity.clone()) {
                    return Err(AppError::Msg(
                        "one USB interface is assigned to multiple hardware roles".into(),
                    ));
                }
            }

            match board.gen {
                3 | 4 => {
                    if board.power.is_some() {
                        return Err(AppError::Msg(
                            "Gen3/Gen4 use relay bindings, not power TTY bindings".into(),
                        ));
                    }

                    let relay = board.relay.as_ref().ok_or_else(|| {
                        AppError::Msg("Gen3/Gen4 board requires relay binding".into())
                    })?;

                    crate::store::validate_serial(&relay.serial)?;

                    if relay.channel > 7
                        || !relay_channels.insert((relay.serial.clone(), relay.channel))
                    {
                        return Err(AppError::Msg(
                            "invalid or duplicate relay/channel assignment".into(),
                        ));
                    }
                }
                5 => {
                    if board.relay.is_some() || board.gpios.is_some() {
                        return Err(AppError::Msg(
                            "Gen5 does not use this service's relay/GPIO binding".into(),
                        ));
                    }

                    let power = board
                        .power
                        .as_ref()
                        .ok_or_else(|| AppError::Msg("Gen5 board requires power binding".into()))?;

                    if board.uart.vid != 0x0403
                        || board.uart.pid != 0x6010
                        || board.uart.interface != 0
                        || power.vid != 0x10c4
                        || power.pid != 0xea60
                        || power.interface != 0
                    {
                        return Err(AppError::Msg(
                            "Gen5 binding does not match the supported USB devices".into(),
                        ));
                    }
                }
                _ => unreachable!(),
            }

            if let Some(offsets) = board.gpios {
                for offset in offsets {
                    if !gpios.insert(offset) {
                        return Err(AppError::Msg("GPIO line is assigned more than once".into()));
                    }
                }
            }

            tracing::debug!(
                mac = %board.mac,
                generation = board.gen,
                uart = ?board.uart,
                relay = ?board.relay,
                power = ?board.power,
                "board binding validated"
            );
        }

        tracing::info!(approved_boards = self.boards.len(), "hardware policy validation succeeded");
        Ok(())
    }

    pub fn board(&self, mac: &str, generation: u8) -> AppResult<&BoardBinding> {
        let mac = crate::store::checked_mac(mac)?;

        self.boards
            .iter()
            .find(|board| board.mac == mac && board.gen == generation)
            .ok_or_else(|| AppError::Msg("board is not approved by hardware policy".into()))
    }

    pub fn relay_allowed(&self, serial: &str, channel: u8) -> bool {
        self.boards.iter().any(|board| {
            board
                .relay
                .as_ref()
                .is_some_and(|relay| relay.serial == serial && relay.channel == channel)
        })
    }

    pub fn power_allowed(&self, tty: &str) -> AppResult<()> {
        for identity in self.boards.iter().filter_map(|board| board.power.as_ref()) {
            if usb::resolve_identity(identity).is_ok_and(|found| found == tty) {
                return Ok(());
            }
        }

        Err(AppError::Msg(
            "power device is not approved by hardware policy".into(),
        ))
    }

    pub fn rtos_device(&self, mac: &str, generation: u8) -> AppResult<String> {
        let board = self.board(mac, generation)?;
        let identity = board
            .rtos
            .as_ref()
            .ok_or_else(|| AppError::Msg("board has no approved RTOS interface".into()))?;

        usb::resolve_identity(identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> HardwarePolicy {
        serde_json::from_value(serde_json::json!({
            "boards": [{
                "mac": "aa:bb:cc:dd:ee:ff",
                "gen": 5,
                "uart": {
                    "vid": 1027,
                    "pid": 24592,
                    "serial": "UART-A",
                    "interface": 0
                },
                "power": {
                    "vid": 4292,
                    "pid": 60000,
                    "serial": "POWER-A",
                    "interface": 0
                }
            }]
        }))
        .unwrap()
    }

    #[test]
    fn valid_policy_normalizes_mac() {
        let mut policy = policy();
        policy.validate().unwrap();
        assert_eq!(policy.boards[0].mac, "aabbccddeeff");
    }

    #[test]
    fn duplicate_board_is_rejected() {
        let mut policy = policy();
        policy.boards.push(policy.boards[0].clone());
        assert!(policy.validate().is_err());
    }

    #[test]
    fn shared_interface_roles_are_rejected() {
        let mut policy = policy();
        policy.boards[0].rtos = Some(policy.boards[0].uart.clone());
        assert!(policy.validate().is_err());
    }

    #[test]
    fn unexpected_gen5_interface_is_rejected() {
        let mut policy = policy();
        policy.boards[0].uart.interface = 1;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn empty_serial_is_rejected() {
        let mut policy = policy();
        policy.boards[0].uart.serial.clear();
        assert!(policy.validate().is_err());
    }
}
