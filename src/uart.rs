//! Linux serial helpers and a bounded, buffered request/response channel.

use crate::{
    constants::LOG_ROOT,
    error::{AppError, AppResult},
};
use serialport::SerialPort;
use tracing::{error, info};

use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::Path,
    time::{Duration, Instant},
};

/// Open an 8-N-1 serial console with bounded read timeouts.
pub fn open_uart(path: &str, baud: u32) -> AppResult<Box<dyn SerialPort>> {
    let _api_trace = crate::observability::ApiTrace::enter("uart", "open_uart");
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
    let _api_trace = crate::observability::ApiTrace::enter("uart", "write_to_path");
    let mut file = OpenOptions::new().write(true).open(path)?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

/// Buffered FlashWriter serial channel with bounded prompt matching.
pub struct Console {
    port: Box<dyn SerialPort>,
    pending: Vec<u8>,
    uart_log: Option<File>,
}

impl Console {
    pub fn open(path: &str, baud: u32, uart_log_prefix: Option<&str>) -> AppResult<Self> {
        let _api_trace = crate::observability::ApiTrace::enter("uart", "open");
        let port = open_uart(path, baud)?;

        let uart_log = uart_log_prefix
            .map(|prefix| {
                let log_path = Path::new(LOG_ROOT).join(format!("{prefix}.uart.log"));

                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&log_path)
                    .map_err(|error| {
                        AppError::Msg(format!("opening UART log {log_path:?} failed: {error}"))
                    })
            })
            .transpose()?;

        Ok(Self {
            port,
            pending: Vec::with_capacity(4096),
            uart_log,
        })
    }

    pub fn write_uart_log(&mut self, bytes: &[u8]) -> AppResult<()> {
        let _api_trace = crate::observability::ApiTrace::enter("uart", "write_uart_log");
        if let Some(log) = &mut self.uart_log {
            log.write_all(bytes)?;
            log.flush()?;
        }
        Ok(())
    }

    pub fn clear_input(&mut self) -> AppResult<()> {
        let _api_trace = crate::observability::ApiTrace::enter("uart", "clear_input");
        self.port
            .clear(serialport::ClearBuffer::Input)
            .map_err(|error| AppError::Msg(format!("clearing UART input failed: {error}")))?;

        self.pending.clear();
        Ok(())
    }

    pub fn send(&mut self, bytes: &[u8]) -> AppResult<()> {
        let _api_trace = crate::observability::ApiTrace::enter("uart", "send");
        if let Err(err) = self.port.write_all(bytes) {
            error!("UART write failed: {err}");
            self.write_uart_log(format!("UART write failed: {err}").as_bytes())?;
            return Err(AppError::Msg(format!("UART write failed: {err}")));
        }
        if let Err(err) = self.port.flush() {
            error!("UART flush failed: {err}");
            self.write_uart_log(format!("UART flush failed: {err}").as_bytes())?;
            return Err(AppError::Msg(format!("UART flush failed: {err}")));
        }
        Ok(())
    }

    pub fn command(&mut self, command: &str) -> AppResult<()> {
        let _api_trace = crate::observability::ApiTrace::enter("uart", "command");
        self.write_uart_log(format!("UART command: {command}").as_bytes())?;
        self.send(command.as_bytes())?;

        // Preserve command pacing expected by the legacy FlashWriter.
        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    }

    pub fn wait(&mut self, expected: &[u8], timeout: Duration) -> AppResult<()> {
        let _api_trace = crate::observability::ApiTrace::enter("uart", "wait");
        self.wait_any(&[expected], timeout).map(|_| ())
    }

    /// Consume through the earliest matching prompt, preserving its suffix.
    ///
    /// Returns the index in `patterns`. If matches begin at the same byte,
    /// the first listed pattern wins.
    pub fn wait_any(&mut self, patterns: &[&[u8]], timeout: Duration) -> AppResult<usize> {
        let _api_trace = crate::observability::ApiTrace::enter("uart", "wait_any");
        if patterns.is_empty()
            || patterns
                .iter()
                .any(|pattern| pattern.is_empty() || pattern.len() > 4096)
        {
            return Err(AppError::Msg("invalid UART prompt specification".into()));
        }

        self.write_uart_log(
            format!("UART wait_any called with patterns: {:?}", patterns).as_bytes(),
        )?;
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
                self.write_uart_log(
                    format!(
                        "UART wait_any timeout. expected one of the patterns: {:?}",
                        patterns
                    )
                    .as_bytes(),
                )?;
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
                    self.write_uart_log(&input[..count])?;
                    self.pending.extend_from_slice(&input[..count]);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    self.write_uart_log(format!("UART read error: {error}").as_bytes())?;
                }
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
        let _api_trace = crate::observability::ApiTrace::enter("uart", "send_srec");
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
                info!("Reached end of S-record file: {}", path.display());
                self.write_uart_log(
                    format!("Reached end of S-record file: {}\n", path.display()).as_bytes(),
                )?;
                break;
            }

            if line.len() > 1024 {
                error!("S-record line exceeds 1024 bytes in {}", path.display());
                self.write_uart_log(
                    format!("S-record line exceeds 1024 bytes in {}\n", path.display()).as_bytes(),
                )?;
                return Err(AppError::Msg(format!(
                    "S-record line exceeds 1024 bytes in {}",
                    path.display()
                )));
            }

            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }

            if !line.starts_with(b"S") {
                error!("invalid S-record line in {}", path.display());
                self.write_uart_log(
                    format!("invalid S-record line in {}\n", path.display()).as_bytes(),
                )?;
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

#[cfg(test)]
mod tests {
    use super::write_to_path;
    use std::fs;

    #[test]
    fn raw_write_preserves_protocol_bytes() {
        let _api_trace = crate::observability::ApiTrace::enter("uart", "raw_write_preserves_protocol_bytes");
        let file = tempfile::NamedTempFile::new().unwrap();
        write_to_path(file.path().to_str().unwrap(), "POWER#ON\n").unwrap();
        assert_eq!(fs::read(file.path()).unwrap(), b"POWER#ON\n");
    }

    #[test]
    fn raw_write_reports_missing_device() {
        let _api_trace = crate::observability::ApiTrace::enter("uart", "raw_write_reports_missing_device");
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing-device");
        assert!(write_to_path(missing.to_str().unwrap(), "POWER#OFF\n").is_err());
    }
}
