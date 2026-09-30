use crate::error::{AppError, AppResult};
use gpio_cdev::{Chip, LineHandle, LineRequestFlags};
use std::sync::Mutex;

/// Retaining the LineHandles retains ownership of the GPIO output lines.
pub struct RequestedLines {
    offsets: [u32; 2],
    handles: [LineHandle; 2],
}

static LINES: Mutex<Option<RequestedLines>> = Mutex::new(None);

pub struct GpioController;

impl GpioController {
    /// Set and retain two GPIO outputs.
    ///
    /// Offsets are gpiochip line offsets, not Raspberry Pi physical header
    /// pin numbers. Confirm the correct gpiochip for your Pi model.
    pub fn set_pair(gpio1: u32, gpio2: u32, value: bool) -> AppResult<()> {
        if gpio1 == gpio2 {
            return Err(AppError::Msg("GPIO offsets must be different".into()));
        }

        let offsets = [gpio1, gpio2];
        let value = u8::from(value);

        let mut lines = LINES
            .lock()
            .map_err(|_| AppError::Msg("GPIO mutex was poisoned".into()))?;

        // If these are already the requested GPIOs, just update their values.
        if let Some(existing) = lines.as_ref() {
            if existing.offsets == offsets {
                existing.handles[0]
                    .set_value(value)
                    .map_err(|e| AppError::Msg(format!("GPIO 1 update failed: {e}")))?;

                existing.handles[1]
                    .set_value(value)
                    .map_err(|e| AppError::Msg(format!("GPIO 2 update failed: {e}")))?;

                return Ok(());
            }
        }

        // Release the old pair before acquiring a possibly overlapping pair.
        *lines = None;

        let device =
            std::env::var("DEV_CONTROLLER_GPIOCHIP").unwrap_or_else(|_| "/dev/gpiochip0".into());

        let mut chip =
            Chip::new(device).map_err(|e| AppError::Msg(format!("GPIO open failed: {e}")))?;

        let handle1 = chip
            .get_line(gpio1)
            .map_err(|e| AppError::Msg(format!("GPIO 1 lookup failed: {e}")))?
            .request(LineRequestFlags::OUTPUT, value, "dev-controller")
            .map_err(|e| AppError::Msg(format!("GPIO 1 request failed: {e}")))?;

        let handle2 = chip
            .get_line(gpio2)
            .map_err(|e| AppError::Msg(format!("GPIO 2 lookup failed: {e}")))?
            .request(LineRequestFlags::OUTPUT, value, "dev-controller")
            .map_err(|e| AppError::Msg(format!("GPIO 2 request failed: {e}")))?;

        *lines = Some(RequestedLines {
            offsets,
            handles: [handle1, handle2],
        });

        Ok(())
    }

    pub fn stop() -> AppResult<()> {
        let mut lines = LINES
            .lock()
            .map_err(|_| AppError::Msg("GPIO mutex was poisoned".into()))?;

        *lines = None;

        Ok(())
    }
}
