//! Linux system metrics for the legacy heartbeat schema.
//!
//! Call heartbeat() from a blocking worker: procfs, sysfs, and filesystem
//! statistics are synchronous operations.

use crate::{
    error::{AppError, AppResult},
    models::HeartbeatPayload,
};

use std::{fs, path::Path, time::Instant};

use sysinfo::Disks;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

#[derive(Default)]
pub struct MetricSampler {
    previous_cpu: Option<(u64, u64)>,
    previous_network: Option<(Instant, u64, u64)>,
}

fn percentage(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

fn parse_counter(value: Option<&str>, name: &str) -> AppResult<u64> {
    value
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| AppError::Msg(format!("invalid Linux metric: {name}")))
}

/// Return (idle including iowait, total CPU ticks).
///
/// Do not add guest/guest_nice: Linux already includes those in user/nice.
fn cpu_ticks() -> AppResult<(u64, u64)> {
    let text = fs::read_to_string("/proc/stat")?;
    let line = text
        .lines()
        .find(|line| line.starts_with("cpu "))
        .ok_or_else(|| AppError::Msg("aggregate CPU counters missing".into()))?;

    let values = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| AppError::Msg("invalid CPU tick counter".into()))
        })
        .collect::<AppResult<Vec<_>>>()?;

    if values.len() < 4 {
        return Err(AppError::Msg("incomplete CPU tick counters".into()));
    }

    let idle = values[3].saturating_add(values.get(4).copied().unwrap_or(0));
    let total = values
        .iter()
        .fold(0_u64, |total, value| total.saturating_add(*value));

    Ok((idle, total))
}

fn frequency_from_sysfs(path: &str) -> Option<f64> {
    let khz = fs::read_to_string(path).ok()?.trim().parse::<f64>().ok()?;

    (khz.is_finite() && khz > 0.0).then_some(khz / 1_000_000.0)
}

fn cpu_frequencies() -> (f64, f64) {
    let current = frequency_from_sysfs("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq")
        .or_else(|| {
            let cpuinfo = fs::read_to_string("/proc/cpuinfo").ok()?;

            cpuinfo.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;

                if key.trim() != "cpu MHz" {
                    return None;
                }

                let mhz = value.trim().parse::<f64>().ok()?;
                (mhz.is_finite() && mhz > 0.0).then_some(mhz / 1000.0)
            })
        })
        .unwrap_or(0.0);

    let maximum = frequency_from_sysfs("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq")
        .unwrap_or(current);

    // Legacy schema has no representation for unavailable frequency.
    // Only these optional frequency fields use zero as unavailable.
    (current, maximum)
}

fn memory_bytes() -> AppResult<(u64, u64)> {
    let text = fs::read_to_string("/proc/meminfo")?;
    let mut total = None;
    let mut available = None;

    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };

        if matches!(key, "MemTotal" | "MemAvailable") {
            let kib = parse_counter(value.split_whitespace().next(), key)?;
            let bytes = kib.saturating_mul(1024);

            match key {
                "MemTotal" => total = Some(bytes),
                "MemAvailable" => available = Some(bytes),
                _ => {}
            }
        }
    }

    let total = total
        .filter(|total| *total > 0)
        .ok_or_else(|| AppError::Msg("MemTotal missing or zero".into()))?;

    let available = available.ok_or_else(|| AppError::Msg("MemAvailable missing".into()))?;

    Ok((total.saturating_sub(available), total))
}

/// Preserve the legacy wired-interface filter.
///
/// This intentionally excludes wlan interfaces. Change the filter here if
/// the backend should report Wi-Fi traffic as well.
fn network_bytes() -> AppResult<(u64, u64)> {
    let text = fs::read_to_string("/proc/net/dev")?;
    let mut received = 0_u64;
    let mut transmitted = 0_u64;

    for line in text.lines().skip(2) {
        let Some((interface, counters)) = line.split_once(':') else {
            continue;
        };

        let interface = interface.trim();
        if !(interface.starts_with("eth") || interface.starts_with("en")) {
            continue;
        }

        let mut fields = counters.split_whitespace();

        let rx = parse_counter(fields.next(), "network RX bytes")?;
        let tx = parse_counter(fields.nth(7), "network TX bytes")?;

        received = received.saturating_add(rx);
        transmitted = transmitted.saturating_add(tx);
    }

    Ok((received, transmitted))
}

fn root_disk_bytes() -> AppResult<(u64, u64)> {
    let disks = Disks::new_with_refreshed_list();

    let root = disks
        .list()
        .iter()
        .find(|disk| disk.mount_point() == Path::new("/"))
        .ok_or_else(|| AppError::Msg("root filesystem statistics unavailable".into()))?;

    let total = root.total_space();
    if total == 0 {
        return Err(AppError::Msg(
            "root filesystem reports zero capacity".into(),
        ));
    }

    Ok((total.saturating_sub(root.available_space()), total))
}

impl MetricSampler {
    pub fn heartbeat(&mut self, uid: String, ip: String) -> AppResult<HeartbeatPayload> {
        // Read all required metrics before advancing either baseline.
        let (idle, total_ticks) = cpu_ticks()?;
        let (received, transmitted) = network_bytes()?;
        let (memory_used, memory_total) = memory_bytes()?;
        let (disk_used, disk_total) = root_disk_bytes()?;
        let (cpu_current, cpu_maximum) = cpu_frequencies();

        let now = Instant::now();

        let cpu_usage = match self.previous_cpu {
            Some((previous_idle, previous_total))
                if total_ticks >= previous_total && idle >= previous_idle =>
            {
                let total_delta = total_ticks - previous_total;
                let idle_delta = idle - previous_idle;
                percentage(total_delta.saturating_sub(idle_delta), total_delta)
            }
            _ => 0.0,
        };

        let (upload, download) = match self.previous_network {
            Some((previous_time, previous_rx, previous_tx)) => {
                let elapsed = now.duration_since(previous_time).as_secs_f64();

                if elapsed > 0.0 {
                    (
                        transmitted.saturating_sub(previous_tx) as f64 * 8.0
                            / elapsed
                            / 1_000_000.0,
                        received.saturating_sub(previous_rx) as f64 * 8.0 / elapsed / 1_000_000.0,
                    )
                } else {
                    (0.0, 0.0)
                }
            }
            None => (0.0, 0.0),
        };

        self.previous_cpu = Some((idle, total_ticks));
        self.previous_network = Some((now, received, transmitted));

        Ok(HeartbeatPayload {
            msg_type: "heartbeat".into(),
            uid,
            ip,
            timestamp: chrono::Utc::now().timestamp(),
            cpu_current: format!("{cpu_current:.1} GHz"),
            cpu_total: format!("{cpu_maximum:.1} GHz"),
            cpu_usage_percent: cpu_usage,
            memory_used: format!("{:.1} GB", memory_used as f64 / GIB),
            memory_total: format!("{:.1} GB", memory_total as f64 / GIB),
            memory_usage_percent: percentage(memory_used, memory_total),
            network_upload: format!("{upload:.1} Mbps"),
            network_download: format!("{download:.1} Mbps"),
            disk_used: format!("{:.1} GB", disk_used as f64 / GIB),
            disk_total: format!("{:.1} GB", disk_total as f64 / GIB),
            disk_usage_percent: percentage(disk_used, disk_total),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::percentage;

    #[test]
    fn percentage_handles_zero_capacity() {
        assert_eq!(percentage(0, 0), 0.0);
    }

    #[test]
    fn percentage_is_bounded() {
        assert_eq!(percentage(25, 100), 25.0);
        assert_eq!(percentage(150, 100), 100.0);
    }
}
