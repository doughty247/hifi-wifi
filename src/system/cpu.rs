//! CPU Monitor with Rolling Average Smoothing
//!
//! Reads /proc/stat to calculate system CPU load with EMA smoothing.
//! Per rewrite.md: Rolling average window size ~3 samples.

use log::debug;
use std::fs;
use std::collections::VecDeque;

/// CPU statistics from /proc/stat
#[derive(Debug, Clone, Default)]
struct CpuTimes {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
    steal: u64,
}

impl CpuTimes {
    /// Parse CPU times from /proc/stat first line
    fn from_proc_stat() -> Option<Self> {
        let content = fs::read_to_string("/proc/stat").ok()?;
        let first_line = content.lines().next()?;
        
        if !first_line.starts_with("cpu ") {
            return None;
        }

        let parts: Vec<u64> = first_line
            .split_whitespace()
            .skip(1) // Skip "cpu" label
            .filter_map(|s| s.parse().ok())
            .collect();

        if parts.len() < 8 {
            return None;
        }

        Some(CpuTimes {
            user: parts[0],
            nice: parts[1],
            system: parts[2],
            idle: parts[3],
            iowait: parts.get(4).copied().unwrap_or(0),
            irq: parts.get(5).copied().unwrap_or(0),
            softirq: parts.get(6).copied().unwrap_or(0),
            steal: parts.get(7).copied().unwrap_or(0),
        })
    }

    /// Total CPU time (all states)
    fn total(&self) -> u64 {
        self.user + self.nice + self.system + self.idle + 
        self.iowait + self.irq + self.softirq + self.steal
    }

    /// Idle time (idle + iowait)
    fn idle_time(&self) -> u64 {
        self.idle + self.iowait
    }
}

/// CPU Monitor with rolling average smoothing
pub struct CpuMonitor {
    /// Previous CPU times for delta calculation
    last_times: Option<CpuTimes>,
    /// Rolling window of CPU load samples
    samples: VecDeque<f64>,
    /// Window size for rolling average
    window_size: usize,
}

impl CpuMonitor {
    pub fn new(window_size: usize) -> Self {
        Self {
            last_times: None,
            samples: VecDeque::with_capacity(window_size),
            window_size,
        }
    }

    /// Sample current CPU load and update rolling average
    /// Returns smoothed CPU load as 0.0-1.0
    pub fn sample(&mut self) -> f64 {
        let current = match CpuTimes::from_proc_stat() {
            Some(t) => t,
            None => return self.smoothed_load(),
        };

        let load = if let Some(ref last) = self.last_times {
            let total_delta = current.total().saturating_sub(last.total());
            let idle_delta = current.idle_time().saturating_sub(last.idle_time());
            
            if total_delta > 0 {
                1.0 - (idle_delta as f64 / total_delta as f64)
            } else {
                0.0
            }
        } else {
            0.0 // First sample, no delta yet
        };

        self.last_times = Some(current);

        // Add to rolling window
        if self.samples.len() >= self.window_size {
            self.samples.pop_front();
        }
        self.samples.push_back(load);

        let smoothed = self.smoothed_load();
        debug!("CPU load: {:.1}% (raw: {:.1}%)", smoothed * 100.0, load * 100.0);
        
        smoothed
    }

    /// Get the smoothed (rolling average) CPU load
    pub fn smoothed_load(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        self.samples.iter().sum::<f64>() / self.samples.len() as f64
    }

}

impl Default for CpuMonitor {
    fn default() -> Self {
        Self::new(3) // Per rewrite.md: window size ~3 samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cpu_monitor_smoothing() {
        let mut monitor = CpuMonitor::new(3);
        
        // Simulate some samples
        monitor.samples.push_back(0.5);
        monitor.samples.push_back(0.6);
        monitor.samples.push_back(0.7);
        
        let avg = monitor.smoothed_load();
        assert!((avg - 0.6).abs() < 0.01);
    }
}
