//! CPU and memory load, sampled from `/proc` for the panel's system monitor.
//!
//! A worker thread reads `/proc/stat` and `/proc/meminfo` every couple of
//! seconds and pushes normalized 0..1 loads to the UI.

use std::sync::mpsc::Receiver;
use std::time::Duration;

/// A system-load sample for the UI (each value in 0.0..=1.0).
#[derive(Debug, Clone, Copy, Default)]
pub struct Load {
    pub cpu: f32,
    pub mem: f32,
}

/// Start the system-load poller. Returns the receiver, or `None` if the thread
/// can't be spawned. Silently does nothing on platforms without `/proc`.
pub fn spawn() -> Option<Receiver<Load>> {
    let (tx, rx) = std::sync::mpsc::channel::<Load>();
    std::thread::Builder::new()
        .name("s-compositor-sysmon".into())
        .spawn(move || {
            let mut prev = read_cpu_times();
            loop {
                std::thread::sleep(Duration::from_secs(2));
                let cur = read_cpu_times();
                let cpu = match (prev, cur) {
                    (Some((pi, pt)), Some((ci, ct))) if ct > pt => {
                        let busy = (ct - pt).saturating_sub(ci.saturating_sub(pi));
                        (busy as f32 / (ct - pt) as f32).clamp(0.0, 1.0)
                    }
                    _ => 0.0,
                };
                prev = cur;
                let mem = read_mem_used().unwrap_or(0.0);
                if tx.send(Load { cpu, mem }).is_err() {
                    break; // UI gone
                }
            }
        })
        .ok()?;
    Some(rx)
}

/// `(idle_jiffies, total_jiffies)` from the aggregate `cpu` line of `/proc/stat`.
fn read_cpu_times() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let mut it = line.split_whitespace();
    if it.next()? != "cpu" {
        return None;
    }
    let fields: Vec<u64> = it.filter_map(|f| f.parse().ok()).collect();
    if fields.len() < 4 {
        return None;
    }
    // user, nice, system, idle, iowait, irq, softirq, steal, ...
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    let total: u64 = fields.iter().sum();
    Some((idle, total))
}

/// Fraction of RAM in use (1 - MemAvailable/MemTotal).
fn read_mem_used() -> Option<f32> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = 0u64;
    let mut avail = 0u64;
    for line in info.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = v.split_whitespace().next()?.parse().ok()?;
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            avail = v.split_whitespace().next()?.parse().ok()?;
        }
    }
    if total == 0 {
        return None;
    }
    Some((1.0 - avail as f32 / total as f32).clamp(0.0, 1.0))
}
