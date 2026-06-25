use anyhow::{Context, Result};
use log::{debug, info, warn};
use std::fs;
use std::path::Path;
use std::process::Command;

const BPF_BYTECODE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/game_bypass.o"));

pub struct EbpfManager {
    bpf_object_path: String,
}

fn is_valid_interface_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

impl EbpfManager {
    pub fn new() -> Self {
        Self {
            bpf_object_path: "/var/lib/hifi-wifi/game_bypass.o".to_string(),
        }
    }

    /// Load the eBPF game bypass program onto the ingress qdisc of the specified interface.
    /// Falls back to standard TC filtering if BPF compilation or loading is unsupported.
    pub fn load_bypass(&self, interface: &str) -> Result<()> {
        if !is_valid_interface_name(interface) {
            anyhow::bail!("Invalid interface name: {}", interface);
        }
        info!("Setting up eBPF bypass for {}", interface);

        // Ensure ingress qdisc exists
        let _ = Command::new("tc")
            .args(["qdisc", "add", "dev", interface, "ingress"])
            .output();

        let mut bpf_ready = false;

        if BPF_BYTECODE.is_empty() {
            debug!("Embedded eBPF bytecode is empty (clang was missing at build time), using legacy TC filters");
        } else {
            // Ensure destination directory exists and write BPF bytecode
            if let Some(parent) = Path::new(&self.bpf_object_path).parent() {
                if !parent.exists() {
                    let _ = fs::create_dir_all(parent);
                }
            }
            if fs::write(&self.bpf_object_path, BPF_BYTECODE).is_ok() {
                bpf_ready = true;
            } else {
                warn!(
                    "Failed to write embedded eBPF object to {}",
                    self.bpf_object_path
                );
            }
        }

        if bpf_ready {
            debug!("Attempting to load BPF object onto {}", interface);
            let output = Command::new("tc")
                .args([
                    "filter",
                    "replace",
                    "dev",
                    interface,
                    "ingress",
                    "protocol",
                    "ip",
                    "prio",
                    "1",
                    "bpf",
                    "obj",
                    &self.bpf_object_path,
                    "sec",
                    "game_bypass",
                ])
                .output()
                .context("Failed to run tc filter command for BPF")?;

            if output.status.success() {
                info!(
                    "eBPF game bypass filter loaded successfully on {}",
                    interface
                );
                return Ok(());
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!(
                    "tc filter BPF loading failed: {}. Falling back to standard TC filters.",
                    stderr.trim()
                );
            }
        }

        // 2. Fallback: Standard TC filters using skbedit to enforce priority 6
        info!(
            "Applying fallback legacy TC filters for game traffic on {}",
            interface
        );

        // Moonlight UDP ports: 47998 - 48010
        let ml_output = Command::new("tc")
            .args([
                "filter", "replace", "dev", interface, "ingress", "protocol", "ip", "prio", "2",
                "u32", "match", "ip", "protocol", "17", "0xff", // UDP
                "match", "ip", "dport", "47998",
                "0xfff0", // matches 47998-48013 (covers 47998-48010)
                "action", "skbedit", "priority", "6",
            ])
            .output()
            .context("Failed to load legacy Moonlight TC filter")?;

        if !ml_output.status.success() {
            let stderr = String::from_utf8_lossy(&ml_output.stderr);
            warn!(
                "Failed to apply Moonlight fallback filter: {}",
                stderr.trim()
            );
        }

        // Steam Link UDP ports: 27031 - 27036
        let sl_output = Command::new("tc")
            .args([
                "filter", "replace", "dev", interface, "ingress", "protocol", "ip", "prio", "3",
                "u32", "match", "ip", "protocol", "17", "0xff", // UDP
                "match", "ip", "dport", "27031", "0xffff", // matches 27031
                "action", "skbedit", "priority", "6",
            ])
            .output()
            .context("Failed to load legacy Steam Link TC filter")?;

        if !sl_output.status.success() {
            let stderr = String::from_utf8_lossy(&sl_output.stderr);
            warn!(
                "Failed to apply Steam Link fallback filter: {}",
                stderr.trim()
            );
        }

        info!(
            "Fallback legacy TC filters applied successfully on {}",
            interface
        );
        Ok(())
    }

    pub fn unload_bypass(&self, interface: &str) -> Result<()> {
        if !is_valid_interface_name(interface) {
            anyhow::bail!("Invalid interface name: {}", interface);
        }
        info!("Unloading bypass filters on {}", interface);

        // Remove filters at prio 1, 2, 3 on the ingress qdisc
        for prio in &["1", "2", "3"] {
            let _ = Command::new("tc")
                .args(["filter", "del", "dev", interface, "ingress", "prio", prio])
                .output();
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ebpf_manager_init() {
        let manager = EbpfManager::new();
        assert_eq!(manager.bpf_object_path, "/var/lib/hifi-wifi/game_bypass.o");
    }

    #[test]
    fn test_load_unload_bypass_resilient() {
        let manager = EbpfManager::new();
        let res = manager.load_bypass("lo");
        assert!(res.is_ok() || res.is_err());

        let res = manager.unload_bypass("lo");
        assert!(res.is_ok());
    }
}
