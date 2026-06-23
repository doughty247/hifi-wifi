//! Firewall Cgroups and Port-based Prioritization
//!
//! Applies DSCP markings (EF) using nftables to ensure the network interface
//! schedules gaming/streaming packets into high-priority queues.

use std::process::Command;
use std::fs;
use std::path::Path;
use log::info;
use anyhow::Result;

/// Dynamically detect active and potential cgroup v2 paths for games and user applications.
pub fn get_candidate_cgroups() -> Vec<String> {
    let mut paths = Vec::new();
    
    // 1. Check gamescope.slice (SteamOS/Bazzite game mode)
    if Path::new("/sys/fs/cgroup/gamescope.slice").exists() {
        paths.push("gamescope.slice".to_string());
    }
    
    // 2. Scan user.slice for user subdirectories (standard desktop distros)
    let user_slice_path = Path::new("/sys/fs/cgroup/user.slice");
    if user_slice_path.exists() {
        if let Ok(entries) = fs::read_dir(user_slice_path) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                // Match user-<uid>.slice
                if name.starts_with("user-") && name.ends_with(".slice") {
                    if let Some(uid_str) = name.strip_prefix("user-").and_then(|s| s.strip_suffix(".slice")) {
                        if uid_str.parse::<u32>().is_ok() {
                            // Standard systemd user applications slice
                            let app_path = format!("user.slice/user-{}.slice/user@{}.service/app.slice", uid_str, uid_str);
                            paths.push(app_path);
                            
                            // Flatpak / transient user scopes (e.g. Steam running under Flatpak or session slices)
                            let session_path = format!("user.slice/user-{}.slice/user@{}.service/session.slice", uid_str, uid_str);
                            paths.push(session_path);
                        }
                    }
                }
            }
        }
    }
    
    // If no paths found, fallback to default UID 1000 path
    if paths.is_empty() {
        paths.push("user.slice/user-1000.slice/user@1000.service/app.slice".to_string());
    }
    
    paths.sort();
    paths.dedup();
    paths
}

pub fn set_dscp_prioritization(enable: bool) -> Result<()> {
    if enable {
        info!("Applying firewall-level game priority tagging (DSCP EF)...");
        
        // 1. Create table
        let _ = Command::new("nft")
            .args(["add", "table", "inet", "hifi_wifi"])
            .output();
        
        // 2. Create postrouting chain
        let _ = Command::new("nft")
            .args(["add", "chain", "inet", "hifi_wifi", "postrouting", "{ type filter hook postrouting priority 0 ; }"])
            .output();

        // 3. Add rules matching detected cgroup paths (UDP only to prevent TCP bulk bloat)
        let candidates = get_candidate_cgroups();
        info!("Detected candidate gaming cgroups: {:?}", candidates);
        for path in candidates {
            let components_count = path.split('/').count().to_string();
            let _ = Command::new("nft")
                .args([
                    "add", "rule", "inet", "hifi_wifi", "postrouting",
                    "socket", "cgroupv2", "level", &components_count, &path,
                    "meta", "l4proto", "udp",
                    "ip", "dscp", "set", "ef",
                    "meta", "priority", "set", "6"
                ])
                .output();
        }

        // 4. Fallback: Port-based prioritization for Steam and common gaming ports
        let _ = Command::new("nft")
            .args([
                "add", "rule", "inet", "hifi_wifi", "postrouting",
                "udp", "dport", "27000-27100", "ip", "dscp", "set", "ef", "meta", "priority", "set", "6"
            ])
            .output();

        // Fallback: Port-based prioritization for Moonlight streaming
        let _ = Command::new("nft")
            .args([
                "add", "rule", "inet", "hifi_wifi", "postrouting",
                "udp", "dport", "47998-48010", "ip", "dscp", "set", "ef", "meta", "priority", "set", "6"
            ])
            .output();

        info!("Firewall-level game priority tagging applied successfully");
    } else {
        info!("Removing firewall-level game priority tagging...");
        let _ = Command::new("nft")
            .args(["delete", "table", "inet", "hifi_wifi"])
            .output();
    }
    Ok(())
}
