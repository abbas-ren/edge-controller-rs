//! Retained Linux GPIO output ownership for the board boot-mode strap.

use crate::error::{AppError, AppResult};
use gpio_cdev::{Chip, LineHandle, LineRequestFlags};
use std::sync::Mutex;

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

        let device =
            std::env::var("DEV_CONTROLLER_GPIOCHIP").unwrap_or_else(|_| "/dev/gpiochip0".into());

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
    use super::GpioController;

    #[test]
    fn stopping_without_requested_lines_is_idempotent() {
        GpioController::stop().unwrap();
        GpioController::stop().unwrap();
    }
}
