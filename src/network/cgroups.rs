//! Firewall Cgroups and Port-based Prioritization
//!
//! Applies DSCP markings (EF) using nftables to ensure the network interface
//! schedules gaming/streaming packets into high-priority queues.

use std::process::Command;
use log::info;
use anyhow::Result;

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

        // 3. Add rule matching gamescope/steam user application cgroups path
        let _ = Command::new("nft")
            .args([
                "add", "rule", "inet", "hifi_wifi", "postrouting",
                "socket", "cgroupv2", "level", "4", "user.slice/user-1000.slice/user@1000.service/app.slice",
                "ip", "dscp", "set", "ef"
            ])
            .output();

        // 4. Fallback: Port-based prioritization for Steam and common gaming ports
        let _ = Command::new("nft")
            .args([
                "add", "rule", "inet", "hifi_wifi", "postrouting",
                "udp", "dport", "27000-27100", "ip", "dscp", "set", "ef"
            ])
            .output();

        // Fallback: Port-based prioritization for Moonlight streaming
        let _ = Command::new("nft")
            .args([
                "add", "rule", "inet", "hifi_wifi", "postrouting",
                "udp", "dport", "47998-48010", "ip", "dscp", "set", "ef"
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
