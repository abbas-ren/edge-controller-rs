//! Linux serial helpers and a bounded, buffered request/response channel.

use crate::error::{AppError, AppResult};
use serialport::SerialPort;

use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::Path,
    time::{Duration, Instant},
};

pub fn open_uart(path: &str, baud: u32) -> AppResult<Box<dyn SerialPort>> {
    serialport::new(path, baud)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .flow_control(serialport::FlowControl::None)
        .timeout(Duration::from_millis(500))
        .open()
        .map_err(|error| {
            AppError::Msg(format!(
                "opening UART {path} at {baud} baud failed: {error}"
            ))
        })
}

/// Raw power-controller write, matching the legacy device protocol.
///
/// Callers must validate/allowlist the pathname before invoking this.
/// Existing terminal settings are preserved because the supplied sources
/// do not specify the power-controller baud rate.
pub fn write_to_path(path: &str, text: &str) -> AppResult<()> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

pub struct Console {
    port: Box<dyn SerialPort>,
    pending: Vec<u8>,
}

impl Console {
    pub fn open(path: &str, baud: u32) -> AppResult<Self> {
        Ok(Self {
            port: open_uart(path, baud)?,
            pending: Vec::with_capacity(4096),
        })
    }

    pub fn clear_input(&mut self) -> AppResult<()> {
        self.port
            .clear(serialport::ClearBuffer::Input)
            .map_err(|error| AppError::Msg(format!("clearing UART input failed: {error}")))?;

        self.pending.clear();
        Ok(())
    }

    pub fn send(&mut self, bytes: &[u8]) -> AppResult<()> {
        self.port.write_all(bytes)?;
        self.port.flush()?;
        Ok(())
    }

    pub fn command(&mut self, command: &str) -> AppResult<()> {
        self.send(command.as_bytes())?;

        // Preserve command pacing expected by the legacy FlashWriter.
        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    }

    pub fn wait(&mut self, expected: &[u8], timeout: Duration) -> AppResult<()> {
        self.wait_any(&[expected], timeout).map(|_| ())
    }

    /// Consume through the earliest matching prompt, preserving its suffix.
    ///
    /// Returns the index in `patterns`. If matches begin at the same byte,
    /// the first listed pattern wins.
    pub fn wait_any(&mut self, patterns: &[&[u8]], timeout: Duration) -> AppResult<usize> {
        if patterns.is_empty()
            || patterns
                .iter()
                .any(|pattern| pattern.is_empty() || pattern.len() > 4096)
        {
            return Err(AppError::Msg("invalid UART prompt specification".into()));
        }

        let deadline = Instant::now() + timeout;
        let keep = patterns
            .iter()
            .map(|pattern| pattern.len())
            .max()
            .unwrap_or(1)
            - 1;

        let mut input = [0_u8; 1024];

        loop {
            let matched = patterns
                .iter()
                .enumerate()
                .filter_map(|(index, pattern)| {
                    self.pending
                        .windows(pattern.len())
                        .position(|part| part == *pattern)
                        .map(|position| (position, index, pattern.len()))
                })
                .min_by_key(|(position, index, _)| (*position, *index));

            if let Some((position, index, length)) = matched {
                self.pending.drain(..position + length);
                return Ok(index);
            }

            if Instant::now() >= deadline {
                return Err(AppError::Msg(format!(
                    "UART prompt timeout; expected one of {:?}",
                    patterns
                        .iter()
                        .map(|pattern| String::from_utf8_lossy(pattern))
                        .collect::<Vec<_>>()
                )));
            }

            if self.pending.len() > 8192 {
                let discard = self.pending.len().saturating_sub(keep);
                self.pending.drain(..discard);
            }

            match self.port.read(&mut input) {
                Ok(0) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(count) => {
                    self.pending.extend_from_slice(&input[..count]);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Send one bounded S-record line at a time.
    ///
    /// The original implementation pauses 10 ms between records.
    /// Removing this pacing without validating the receiver can overrun
    /// its serial input buffer.
    pub fn send_srec(&mut self, path: &Path) -> AppResult<()> {
        use std::io::Read;

        let mut reader = BufReader::new(File::open(path)?);
        let mut line = Vec::with_capacity(1024);

        loop {
            line.clear();

            // Cap each record before allocating unbounded memory.
            let count = {
                let mut bounded = (&mut reader).take(1025);
                bounded.read_until(b'\n', &mut line)?
            };

            if count == 0 {
                break;
            }

            if line.len() > 1024 {
                return Err(AppError::Msg(format!(
                    "S-record line exceeds 1024 bytes in {}",
                    path.display()
                )));
            }

            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }

            if !line.starts_with(b"S") {
                return Err(AppError::Msg(format!(
                    "invalid S-record line in {}",
                    path.display()
                )));
            }

            self.send(&line)?;
            std::thread::sleep(Duration::from_millis(10));
        }

        Ok(())
    }
}
