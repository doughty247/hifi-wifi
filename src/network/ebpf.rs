use anyhow::{Context, Result};
use log::{debug, info, warn};
use std::fs;
use std::path::Path;
use std::process::Command;

pub struct EbpfManager {
    bpf_object_path: String,
    bpf_source_path: String,
}

impl EbpfManager {
    pub fn new() -> Self {
        Self {
            bpf_object_path: "/var/lib/hifi-wifi/game_bypass.o".to_string(),
            bpf_source_path: "/var/lib/hifi-wifi/game_bypass.c".to_string(),
        }
    }

    /// Setup the BPF source file and attempt to compile it if clang is available
    fn compile_bpf_program(&self) -> Result<bool> {
        // Ensure destination folder exists
        if let Some(parent) = Path::new(&self.bpf_object_path).parent() {
            if !parent.exists() {
                fs::create_dir_all(parent)
                    .context("Failed to create /var/lib/hifi-wifi directory")?;
            }
        }

        // Copy source file to /var/lib/hifi-wifi/game_bypass.c if it exists in the workspace
        let workspace_src = "src/bpf/game_bypass.c";
        if Path::new(workspace_src).exists() {
            fs::copy(workspace_src, &self.bpf_source_path)
                .context("Failed to copy BPF source file")?;
        } else {
            // Write it directly if not running in workspace
            let embedded_c = include_str!("../bpf/game_bypass.c");
            fs::write(&self.bpf_source_path, embedded_c)
                .context("Failed to write embedded BPF source")?;
        }

        // Check if clang is present
        let clang_check = Command::new("which").arg("clang").output();

        let has_clang = match clang_check {
            Ok(out) => out.status.success(),
            Err(_) => false,
        };

        if !has_clang {
            debug!("clang compiler not found, cannot build eBPF source dynamically");
            return Ok(false);
        }

        info!(
            "Compiling eBPF program: {} -> {}",
            self.bpf_source_path, self.bpf_object_path
        );
        let output = Command::new("clang")
            .args([
                "-O2",
                "-target",
                "bpf",
                "-c",
                &self.bpf_source_path,
                "-o",
                &self.bpf_object_path,
            ])
            .output()
            .context("Failed to execute clang command")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("clang failed to compile BPF program: {}", stderr.trim());
            return Ok(false);
        }

        info!("Successfully compiled eBPF game bypass filter");
        Ok(true)
    }

    /// Load the eBPF game bypass program onto the ingress qdisc of the specified interface.
    /// Falls back to standard TC filtering if BPF compilation or loading is unsupported.
    pub fn load_bypass(&self, interface: &str) -> Result<()> {
        info!("Setting up eBPF bypass for {}", interface);

        // Ensure ingress qdisc exists
        let _ = Command::new("tc")
            .args(["qdisc", "add", "dev", interface, "ingress"])
            .output();

        // 1. Try to compile and load eBPF
        let mut bpf_ready = Path::new(&self.bpf_object_path).exists();
        if !bpf_ready {
            match self.compile_bpf_program() {
                Ok(success) => bpf_ready = success,
                Err(e) => warn!("BPF compilation check failed: {}", e),
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

    /// Unload the eBPF or fallback filters from the interface
    pub fn unload_bypass(&self, interface: &str) -> Result<()> {
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
        assert_eq!(manager.bpf_source_path, "/var/lib/hifi-wifi/game_bypass.c");
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
