//! Process / system statistics for the Lavalink `stats` payload (Linux `/proc`, zeros
//! elsewhere).

use std::sync::Mutex;

use crate::api::models::{Cpu, Memory};

#[derive(Debug, Clone, Copy, Default)]
struct CpuSample {
    total: u64,
    idle: u64,
    process: u64,
}

#[derive(Default)]
pub struct SystemSampler {
    previous: Mutex<Option<CpuSample>>,
}

impl SystemSampler {
    pub fn memory(&self) -> Memory {
        let rss = read_status_kib("VmRSS").unwrap_or(0) * 1024;
        let (total, available) = read_meminfo();
        Memory {
            free: available,
            used: rss,
            allocated: rss,
            reservable: total,
        }
    }

    /// CPU load since the previous call (0.0 - 1.0).
    pub fn cpu(&self) -> Cpu {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1);
        let Some(current) = read_cpu_sample() else {
            return Cpu {
                cores,
                ..Default::default()
            };
        };
        let mut previous = self.previous.lock().unwrap();
        let (system_load, process_load) = match *previous {
            Some(prev) if current.total > prev.total => {
                let total = (current.total - prev.total) as f64;
                let idle = current.idle.saturating_sub(prev.idle) as f64;
                let process = current.process.saturating_sub(prev.process) as f64;
                ((1.0 - idle / total).clamp(0.0, 1.0), (process / total).clamp(0.0, 1.0))
            }
            _ => (0.0, 0.0),
        };
        *previous = Some(current);
        Cpu {
            cores,
            system_load,
            lavalink_load: process_load,
        }
    }
}

fn read_status_kib(key: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|l| l.starts_with(key))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn read_meminfo() -> (u64, u64) {
    let Ok(info) = std::fs::read_to_string("/proc/meminfo") else {
        return (0, 0);
    };
    let field = |key: &str| {
        info.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    (field("MemTotal:"), field("MemAvailable:"))
}

fn read_cpu_sample() -> Option<CpuSample> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let values: Vec<u64> = stat
        .lines()
        .next()?
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    if values.len() < 4 {
        return None;
    }
    let total: u64 = values.iter().take(8).sum();
    let idle = values[3] + values.get(4).copied().unwrap_or(0);

    let own = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the command name, which may itself contain spaces and parentheses.
    let after_comm = own.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(CpuSample {
        total,
        idle,
        process: utime + stime,
    })
}
