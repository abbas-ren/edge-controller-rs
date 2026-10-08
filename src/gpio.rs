//! Retained Linux GPIO output ownership for the board boot-mode strap.

use crate::error::{AppError, AppResult};
use gpio_cdev::{Chip, LineHandle, LineRequestFlags};
use std::sync::Mutex;

/// Usable 40-pin header positions and their gpiochip line offsets.
pub const HEADER_PINS: [(u8, u8); 28] = [
    (3, 2),
    (5, 3),
    (7, 4),
    (8, 14),
    (10, 15),
    (11, 17),
    (12, 18),
    (13, 27),
    (15, 22),
    (16, 23),
    (18, 24),
    (19, 10),
    (21, 9),
    (22, 25),
    (23, 11),
    (24, 8),
    (26, 7),
    (27, 0),
    (28, 1),
    (29, 5),
    (31, 6),
    (32, 12),
    (33, 13),
    (35, 19),
    (36, 16),
    (37, 26),
    (38, 20),
    (40, 21),
];

/// Return the conventional GPIO character device for a Raspberry Pi model.
pub fn default_device(raspberry_pi_model: u8) -> &'static str {
    match raspberry_pi_model {
        5 => "/dev/gpiochip4",
        _ => "/dev/gpiochip0",
    }
}

/// Retaining the LineHandle retains ownership of the GPIO output line.
pub struct RequestedLine {
    offset: u32,
    handle: LineHandle,
}

static LINE: Mutex<Option<RequestedLine>> = Mutex::new(None);

/// Process-wide controller for the retained GPIO line.
pub struct GpioController;

impl GpioController {
    /// Set and retain one GPIO output.
    ///
    /// Offsets are gpiochip line offsets, not Raspberry Pi physical header
    /// pin numbers. Confirm the correct gpiochip for your Pi model.
    pub fn set(gpio: u32, value: bool) -> AppResult<()> {
        let value = u8::from(value);

        let mut line = LINE
            .lock()
            .map_err(|_| AppError::Msg("GPIO mutex was poisoned".into()))?;

        if let Some(existing) = line.as_ref() {
            if existing.offset == gpio {
                existing
                    .handle
                    .set_value(value)
                    .map_err(|e| AppError::Msg(format!("GPIO update failed: {e}")))?;

                return Ok(());
            }
        }

        *line = None;

        let device = std::env::var("DEV_CONTROLLER_GPIOCHIP").unwrap_or_else(|_| {
            default_device(crate::control::current().raspberry_pi_model).into()
        });

        let mut chip =
            Chip::new(device).map_err(|e| AppError::Msg(format!("GPIO open failed: {e}")))?;

        let handle = chip
            .get_line(gpio)
            .map_err(|e| AppError::Msg(format!("GPIO lookup failed: {e}")))?
            .request(LineRequestFlags::OUTPUT, value, "dev-controller")
            .map_err(|e| AppError::Msg(format!("GPIO request failed: {e}")))?;

        *line = Some(RequestedLine {
            offset: gpio,
            handle,
        });

        Ok(())
    }

    /// Release any retained GPIO line handles.
    pub fn stop() -> AppResult<()> {
        let mut line = LINE
            .lock()
            .map_err(|_| AppError::Msg("GPIO mutex was poisoned".into()))?;

        *line = None;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{default_device, GpioController, HEADER_PINS};

    #[test]
    fn profiles_use_expected_gpio_devices_and_header_offsets() {
        assert_eq!(default_device(4), "/dev/gpiochip0");
        assert_eq!(default_device(5), "/dev/gpiochip4");
        assert!(HEADER_PINS.contains(&(11, 17)));
        assert!(HEADER_PINS.contains(&(40, 21)));
    }

    #[test]
    fn stopping_without_requested_lines_is_idempotent() {
        GpioController::stop().unwrap();
        GpioController::stop().unwrap();
    }
}
