//! TCP socket info telemetry module
//!
//! Queries the kernel tcp_info struct of active socket connections
//! owned by gaming and streaming processes to monitor RTT and jitter in real-time.

use anyhow::Result;
use nix::libc;
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct SocketTelemetry {
    pub rtt_us: u32,
    pub rtt_var_us: u32,
    pub retransmits: u32,
    pub lost_packets: u32,
}

/// Query TCP telemetry for a specific process ID
pub fn get_process_tcp_telemetry(pid: u32) -> Result<Vec<SocketTelemetry>> {
    let mut results = Vec::new();
    let fd_dir = format!("/proc/{}/fd", pid);

    if !Path::new(&fd_dir).exists() {
        return Ok(results);
    }

    let entries = match fs::read_dir(&fd_dir) {
        Ok(e) => e,
        Err(_) => return Ok(results), // Process might have terminated
    };

    for entry in entries.flatten() {
        let path = entry.path();

        // Check if link points to a socket
        let link_target = match fs::read_link(&path) {
            Ok(t) => t.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        if link_target.starts_with("socket:") {
            // Open the /proc/<pid>/fd/<fd> descriptor to get a duplicate socket FD
            if let Ok(file) = fs::File::open(&path) {
                let raw_fd = file.as_raw_fd();

                let mut info: libc::tcp_info = unsafe { std::mem::zeroed() };
                let mut len = std::mem::size_of::<libc::tcp_info>() as libc::socklen_t;

                let ret = unsafe {
                    libc::getsockopt(
                        raw_fd,
                        libc::IPPROTO_TCP,
                        libc::TCP_INFO,
                        &mut info as *mut _ as *mut libc::c_void,
                        &mut len,
                    )
                };

                if ret == 0 {
                    // Check if it's an active connection (state 1 is TCP_ESTABLISHED)
                    if info.tcpi_state == 1 {
                        let telemetry = SocketTelemetry {
                            rtt_us: info.tcpi_rtt,
                            rtt_var_us: info.tcpi_rttvar,
                            retransmits: info.tcpi_retransmits as u32,
                            lost_packets: info.tcpi_lost as u32,
                        };
                        results.push(telemetry);
                    }
                }
            }
        }
    }

    Ok(results)
}

/// Collect telemetry for all gaming and streaming processes
pub fn get_gaming_telemetry() -> Result<SocketTelemetry> {
    let pids = collect_gaming_pids()?;

    let mut total_rtt = 0u64;
    let mut total_rttvar = 0u64;
    let mut total_retrans = 0u32;
    let mut total_lost = 0u32;
    let mut count = 0u32;

    for pid in pids {
        if let Ok(telemetries) = get_process_tcp_telemetry(pid) {
            for t in telemetries {
                // Filter out localhost / loopback metrics (typically RTT < 50us)
                if t.rtt_us > 100 {
                    total_rtt += t.rtt_us as u64;
                    total_rttvar += t.rtt_var_us as u64;
                    total_retrans += t.retransmits;
                    total_lost += t.lost_packets;
                    count += 1;
                }
            }
        }
    }

    if count == 0 {
        return Ok(SocketTelemetry::default());
    }

    Ok(SocketTelemetry {
        rtt_us: (total_rtt / count as u64) as u32,
        rtt_var_us: (total_rttvar / count as u64) as u32,
        retransmits: total_retrans,
        lost_packets: total_lost,
    })
}

/// Collect process IDs for running games, streaming applications, and steam clients
fn collect_gaming_pids() -> Result<Vec<u32>> {
    let mut pids = Vec::new();

    // 1. Try reading PIDs from cgroup user slices (gamescope and app slices)
    let cgroup_base = Path::new("/sys/fs/cgroup/user.slice");
    if cgroup_base.exists() {
        let mut cgroup_paths = Vec::new();
        find_cgroup_procs_paths(cgroup_base, &mut cgroup_paths);

        for path in cgroup_paths {
            if let Ok(content) = fs::read_to_string(path) {
                for line in content.lines() {
                    if let Ok(pid) = line.trim().parse::<u32>() {
                        if is_gaming_process(pid) {
                            pids.push(pid);
                        }
                    }
                }
            }
        }
    }

    // 2. Fallback: Search all active /proc processes if cgroups were empty or unavailable
    if pids.is_empty() {
        if let Ok(entries) = fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Ok(pid) = name.parse::<u32>() {
                    if is_gaming_process(pid) {
                        pids.push(pid);
                    }
                }
            }
        }
    }

    pids.sort();
    pids.dedup();

    Ok(pids)
}

/// Recursively find cgroup.procs paths inside user/app slices
fn find_cgroup_procs_paths(dir: &Path, paths: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let procs_file = path.join("cgroup.procs");
                if procs_file.exists() {
                    // We prioritize gamescope and steam app scopes
                    let path_str = path.to_string_lossy();
                    if path_str.contains("gamescope") || path_str.contains("app.slice") {
                        paths.push(procs_file);
                    }
                }
                find_cgroup_procs_paths(&path, paths);
            }
        }
    }
}

/// Check if a process represents a gaming or streaming app based on name
fn is_gaming_process(pid: u32) -> bool {
    let comm_path = format!("/proc/{}/comm", pid);
    if let Ok(comm) = fs::read_to_string(&comm_path) {
        let comm_lower = comm.trim().to_lowercase();

        // Known game clients, streaming engines, and game processes
        comm_lower.contains("steam")
            || comm_lower.contains("gamescope")
            || comm_lower.contains("moonlight")
            || comm_lower.contains("sunshine")
            || comm_lower.contains("retroarch")
            || comm_lower.contains("wine")
            || comm_lower.contains("proton")
            || comm_lower.contains("heroic")
            || comm_lower.contains("lutris")
            || comm_lower.contains("csgo")
            || comm_lower.contains("dota")
            || comm_lower.contains("hl2")
    } else {
        false
    }
}
