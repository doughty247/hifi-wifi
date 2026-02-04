//! Firmware update module for Steam Deck OLED WiFi (QCA2066/ath11k)
//!
//! This module provides firmware management capabilities:
//! - `status`: Show current vs latest available firmware version
//! - `update`: Download and install latest upstream firmware from linux-firmware.git
//! - `revert`: Restore original stock firmware from backup
//!
//! **Hardware gate**: Only runs on Steam Deck OLED (Galileo) with QCA2066 WiFi card.

pub mod device;
pub mod version;
pub mod download;
pub mod deploy;

use anyhow::{Result, bail, Context};
use clap::Subcommand;

use crate::firmware::device::DeviceInfo;
use crate::firmware::version::{FirmwareVersion, get_upstream_version};
use crate::firmware::deploy::{BackupManager, FirmwareDeployer, is_steamos, disable_readonly, enable_readonly};
use crate::firmware::download::FirmwareDownloader;

/// Firmware subcommands
#[derive(Subcommand, Clone)]
pub enum FirmwareAction {
    /// Show current and latest available firmware version
    Status {
        /// Output as JSON for scripting
        #[arg(long)]
        json: bool,
        /// Skip fetching upstream version (offline mode)
        #[arg(long)]
        offline: bool,
    },
    /// Download and install latest upstream firmware
    Update {
        /// Skip confirmation prompts
        #[arg(long, short = 'y')]
        force: bool,
    },
    /// Revert to original stock firmware from backup
    Revert {
        /// Skip confirmation prompts
        #[arg(long, short = 'y')]
        force: bool,
    },
}

/// Main entry point for firmware commands
pub fn run_firmware(action: FirmwareAction, dry_run: bool) -> Result<()> {
    match action {
        FirmwareAction::Status { json, offline } => run_status(json, offline),
        FirmwareAction::Update { force } => run_update(force, dry_run),
        FirmwareAction::Revert { force } => run_revert(force, dry_run),
    }
}

/// ANSI color codes
mod colors {
    pub const RED: &str = "\x1b[0;31m";
    pub const GREEN: &str = "\x1b[0;32m";
    pub const YELLOW: &str = "\x1b[0;33m";
    pub const CYAN: &str = "\x1b[0;36m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const NC: &str = "\x1b[0m";
}

/// Run firmware status check
fn run_status(json: bool, offline: bool) -> Result<()> {
    use colors::*;

    // Detect device (informational only for status - don't gate)
    let device = DeviceInfo::detect();

    // Get firmware path and current version
    let firmware_path = version::detect_firmware_path()?;
    let current = FirmwareVersion::from_installed(&firmware_path)?;

    // Check for interrupted update
    let health_warnings = check_health(&firmware_path);

    // Get upstream version (unless offline mode)
    let upstream = if offline {
        None
    } else {
        match get_upstream_version() {
            Ok(v) => Some(v),
            Err(e) => {
                if !json {
                    eprintln!("{}Warning:{} Could not fetch upstream version: {}", YELLOW, NC, e);
                }
                None
            }
        }
    };

    // Check backup status
    let backup_mgr = BackupManager::new(&firmware_path);
    let backup_info = backup_mgr.get_backup_info();

    if json {
        // JSON output for scripting
        let output = serde_json::json!({
            "device": {
                "supported": device.is_supported(),
                "board_vendor": device.board_vendor,
                "board_name": device.board_name,
                "wifi_vendor": device.wifi_vendor,
                "wifi_device": device.wifi_device,
            },
            "firmware": {
                "path": firmware_path.to_string_lossy(),
                "current_version": current.version_string,
                "is_valve_stock": current.is_valve_stock(),
            },
            "upstream": upstream.as_ref().map(|u| &u.version_string),
            "backup": backup_info.as_ref().map(|b| serde_json::json!({
                "version": b.version,
                "date": b.backup_date.to_rfc3339(),
                "is_valve_stock": b.is_valve_stock,
            })),
            "update_available": upstream.as_ref().map(|u| u.version_string != current.version_string),
            "health_warnings": health_warnings,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    // Human-readable output
    println!();
    println!("{}{}WiFi Firmware Status{}", BOLD, CYAN, NC);
    println!("{}═══════════════════════════════════════{}", CYAN, NC);
    println!();

    // Device info
    if device.is_supported() {
        println!("{}Device:{}\t\tSteam Deck OLED (Galileo) {}✓{}", BOLD, NC, GREEN, NC);
    } else {
        println!("{}Device:{}\t\t{} {} {}[Not Supported]{}", BOLD, NC,
                 device.board_vendor.as_deref().unwrap_or("Unknown"),
                 device.board_name.as_deref().unwrap_or("Unknown"),
                 RED, NC);
    }

    // WiFi card info
    if device.is_wifi_supported() {
        println!("{}WiFi Card:{}\t\tQCA2066 (17cb:1103) {}✓{}", BOLD, NC, GREEN, NC);
    } else {
        println!("{}WiFi Card:{}\t\t{}:{} {}[Not Supported]{}", BOLD, NC,
                 device.wifi_vendor.as_deref().unwrap_or("????"),
                 device.wifi_device.as_deref().unwrap_or("????"),
                 RED, NC);
    }

    println!("{}Firmware Path:{}\t{}", BOLD, NC, firmware_path.display());
    println!();

    // Health warnings
    for warning in &health_warnings {
        println!("{}⚠ Warning:{} {}", YELLOW, NC, warning);
    }
    if !health_warnings.is_empty() {
        println!();
    }

    // Version info
    let stock_indicator = if current.is_valve_stock() {
        format!(" {}(Valve stock){}", DIM, NC)
    } else {
        format!(" {}(upstream){}", DIM, NC)
    };
    println!("{}Current:{}\t\t{}{}", BOLD, NC, current.version_string, stock_indicator);

    if let Some(ref upstream_ver) = upstream {
        println!("{}Latest upstream:{}\t{}", BOLD, NC, upstream_ver.version_string);

        // Status
        if upstream_ver.version_string == current.version_string {
            println!("{}Status:{}\t\t{}Up to date{}", BOLD, NC, GREEN, NC);
        } else if current.is_valve_stock() {
            println!("{}Status:{}\t\t{}Update available{}", BOLD, NC, YELLOW, NC);
        } else {
            // Already on upstream but different version
            println!("{}Status:{}\t\t{}Newer version available{}", BOLD, NC, YELLOW, NC);
        }
    }

    // Backup info
    if let Some(ref backup) = backup_info {
        let stock_str = if backup.is_valve_stock { " (Valve stock)" } else { "" };
        println!("{}Backup:{}\t\t{}{} ({})", BOLD, NC, backup.version, stock_str,
                 backup.backup_date.format("%Y-%m-%d"));
    } else {
        println!("{}Backup:{}\t\t{}None{}", BOLD, NC, DIM, NC);
    }

    println!();

    // Helpful hints
    if !device.is_supported() {
        println!("{}Note:{} Firmware updates are only available on Steam Deck OLED.", DIM, NC);
    } else if upstream.as_ref().map(|u| u.version_string != current.version_string).unwrap_or(false) {
        println!("{}Tip:{} Run 'sudo hifi-wifi firmware update' to install the latest firmware.", DIM, NC);
    }

    Ok(())
}

/// Check for health issues (interrupted updates, missing files, etc.)
fn check_health(firmware_path: &std::path::Path) -> Vec<String> {
    let mut warnings = Vec::new();

    // Check for .new files (interrupted update)
    for file in &["amss.bin.zst.new", "m3.bin.zst.new", "board-2.bin.zst.new"] {
        if firmware_path.join(file).exists() {
            warnings.push("Interrupted update detected. Run 'hifi-wifi firmware update' to complete.".to_string());
            break;
        }
    }

    // Check for backup without metadata
    let backup_mgr = BackupManager::new(firmware_path);
    if backup_mgr.backup_files_exist() && backup_mgr.get_backup_info().is_none() {
        warnings.push("Backup metadata missing. Integrity cannot be verified.".to_string());
    }

    warnings
}

/// Run firmware update
fn run_update(force: bool, dry_run: bool) -> Result<()> {
    use colors::*;

    println!();
    println!("{}{}WiFi Firmware Update{}", BOLD, CYAN, NC);
    println!("{}═══════════════════════════════════════{}", CYAN, NC);
    println!();

    // Phase 1: Pre-flight checks
    println!("{}[1/5]{} Pre-flight checks...", DIM, NC);

    // Hardware gate
    let device = DeviceInfo::detect();
    if !device.is_supported() {
        bail!(
            "Firmware updates are only supported on Steam Deck OLED (Galileo) with QCA2066 WiFi.\n\
             Detected device: {} {}\n\
             Detected WiFi:   {}:{} (subsystem {}:{})",
            device.board_vendor.as_deref().unwrap_or("Unknown"),
            device.board_name.as_deref().unwrap_or("Unknown"),
            device.wifi_vendor.as_deref().unwrap_or("????"),
            device.wifi_device.as_deref().unwrap_or("????"),
            device.wifi_subsys_vendor.as_deref().unwrap_or("????"),
            device.wifi_subsys_device.as_deref().unwrap_or("????"),
        );
    }
    println!("  Device: Steam Deck OLED {}✓{}", GREEN, NC);
    println!("  WiFi:   QCA2066 (17cb:1103) {}✓{}", GREEN, NC);

    // Firmware path
    let firmware_path = version::detect_firmware_path()?;
    println!("  Path:   {} {}✓{}", firmware_path.display(), GREEN, NC);

    // Current version
    let current = FirmwareVersion::from_installed(&firmware_path)?;
    println!("  Current: {}", current.version_string);

    // Upstream version
    let upstream = get_upstream_version()?;
    println!("  Latest:  {}", upstream.version_string);

    // Check if update needed
    if current.version_string == upstream.version_string && !force {
        println!();
        println!("{}Already running the latest firmware. Nothing to do.{}", GREEN, NC);
        return Ok(());
    }

    // Check disk space (need ~25MB)
    check_disk_space(&firmware_path, 25 * 1024 * 1024)?;
    println!("  Disk:   Sufficient space {}✓{}", GREEN, NC);

    if dry_run {
        println!();
        println!("{}[DRY-RUN]{} Would download and install firmware.", YELLOW, NC);
        println!("  Files: amss.bin, m3.bin, board-2.bin");
        println!("  From:  linux-firmware.git (GitLab)");
        return Ok(());
    }

    // Phase 2: Download to staging
    println!();
    println!("{}[2/5]{} Downloading firmware...", DIM, NC);

    let downloader = FirmwareDownloader::new()?;
    let staging_dir = downloader.download_all()?;
    println!("  Downloaded to staging {}✓{}", GREEN, NC);

    // Validate downloads
    println!();
    println!("{}[3/5]{} Validating downloads...", DIM, NC);
    downloader.validate(&staging_dir)?;
    println!("  All files validated {}✓{}", GREEN, NC);

    // Phase 3: Backup (if needed)
    println!();
    println!("{}[4/5]{} Managing backup...", DIM, NC);

    // Handle SteamOS readonly filesystem for backup and deploy
    let steamos = is_steamos();
    if steamos {
        disable_readonly()?;
    }

    // Use a closure to ensure we re-enable readonly even on error
    let result = (|| -> Result<()> {
        let backup_mgr = BackupManager::new(&firmware_path);
        if !backup_mgr.backup_files_exist() {
            // First update - create backup
            if !current.is_valve_stock() && !force {
                println!();
                println!("{}Warning:{} Current firmware is not Valve stock.", YELLOW, NC);
                println!("  Current: {}", current.version_string);
                println!("  Expected: CI_WLAN.HSP.1.1-... (Valve prefix)");
                println!();
                println!("Creating backup of current (modified) state. To restore true Valve");
                println!("stock firmware, use SteamOS recovery or reinstall.");
                println!();

                if !confirm("Continue with backup and update?")? {
                    bail!("Update cancelled by user.");
                }
            }

            backup_mgr.create_backup(&current)?;
            println!("  Backup created {}✓{}", GREEN, NC);
        } else {
            println!("  Backup already exists {}✓{}", GREEN, NC);
        }

        // Phase 4: Deploy
        println!();
        println!("{}[5/5]{} Deploying firmware...", DIM, NC);

        let deployer = FirmwareDeployer::new(&firmware_path);
        deployer.deploy(&staging_dir)?;
        println!("  Firmware deployed {}✓{}", GREEN, NC);

        Ok(())
    })();

    // Re-enable readonly regardless of success/failure
    if steamos {
        if let Err(e) = enable_readonly() {
            eprintln!("{}Warning:{} Failed to re-enable readonly: {}", YELLOW, NC, e);
        }
    }

    // Propagate any error from the update process
    result?;

    // Verify
    let new_version = FirmwareVersion::from_installed(&firmware_path)?;

    // Cleanup staging
    let _ = std::fs::remove_dir_all(&staging_dir);

    println!();
    println!("{}═══════════════════════════════════════{}", GREEN, NC);
    println!("{}Firmware updated successfully!{}", GREEN, NC);
    println!("  Previous: {}", current.version_string);
    println!("  Current:  {}", new_version.version_string);
    println!();
    println!("{}⚠ Reboot required to load new firmware.{}", YELLOW, NC);
    println!();

    if confirm("Reboot now?")? {
        println!("Rebooting...");
        std::process::Command::new("reboot")
            .status()
            .context("Failed to reboot")?;
    }

    Ok(())
}

/// Run firmware revert
fn run_revert(force: bool, dry_run: bool) -> Result<()> {
    use colors::*;

    println!();
    println!("{}{}WiFi Firmware Revert{}", BOLD, CYAN, NC);
    println!("{}═══════════════════════════════════════{}", CYAN, NC);
    println!();

    // Hardware gate
    let device = DeviceInfo::detect();
    if !device.is_supported() {
        bail!(
            "Firmware management is only supported on Steam Deck OLED (Galileo) with QCA2066 WiFi.\n\
             Detected device: {} {}",
            device.board_vendor.as_deref().unwrap_or("Unknown"),
            device.board_name.as_deref().unwrap_or("Unknown"),
        );
    }

    // Get paths and versions
    let firmware_path = version::detect_firmware_path()?;
    let current = FirmwareVersion::from_installed(&firmware_path)?;

    // Check backup exists
    let backup_mgr = BackupManager::new(&firmware_path);
    let backup_info = backup_mgr.get_backup_info();

    if !backup_mgr.backup_files_exist() {
        bail!(
            "No backup found. Cannot revert.\n\n\
             This can happen if:\n\
               - You haven't run 'hifi-wifi firmware update' yet\n\
               - The backup files were deleted\n\n\
             To restore Valve stock firmware, use SteamOS recovery or reinstall."
        );
    }

    // Get backup version
    let backup_version = if let Some(ref info) = backup_info {
        info.version.clone()
    } else {
        // No metadata - try to extract version from backup
        backup_mgr.extract_backup_version()?
    };

    // Check if already on backup version
    if current.version_string == backup_version && !force {
        println!("{}Already running backup firmware. Nothing to revert.{}", GREEN, NC);
        return Ok(());
    }

    // Verify backup integrity (if metadata exists)
    if let Some(ref info) = backup_info {
        println!("{}[1/3]{} Verifying backup integrity...", DIM, NC);
        backup_mgr.verify_integrity(info)?;
        println!("  Backup verified {}✓{}", GREEN, NC);
    } else {
        println!("{}[1/3]{} {}Warning:{} No backup metadata. Cannot verify integrity.", DIM, NC, YELLOW, NC);
    }

    // Confirm
    if !force && !dry_run {
        println!();
        let stock_str = backup_info.as_ref()
            .map(|i| if i.is_valve_stock { " (Valve stock)" } else { "" })
            .unwrap_or("");
        let date_str = backup_info.as_ref()
            .map(|i| format!(" from {}", i.backup_date.format("%Y-%m-%d")))
            .unwrap_or_default();

        println!("Current firmware:  {}", current.version_string);
        println!("Backup firmware:   {}{}{}", backup_version, stock_str, date_str);
        println!();

        if !confirm("Revert to backup firmware?")? {
            bail!("Revert cancelled by user.");
        }
    }

    if dry_run {
        println!();
        println!("{}[DRY-RUN]{} Would restore firmware from backup.", YELLOW, NC);
        return Ok(());
    }

    // Restore
    println!();
    println!("{}[2/3]{} Restoring firmware...", DIM, NC);

    let deployer = FirmwareDeployer::new(&firmware_path);
    deployer.restore_backup()?;
    println!("  Firmware restored {}✓{}", GREEN, NC);

    // Verify
    println!();
    println!("{}[3/3]{} Verifying restoration...", DIM, NC);
    let new_version = FirmwareVersion::from_installed(&firmware_path)?;
    println!("  Version: {} {}✓{}", new_version.version_string, GREEN, NC);

    println!();
    println!("{}═══════════════════════════════════════{}", GREEN, NC);
    println!("{}Firmware reverted successfully!{}", GREEN, NC);
    println!("  Previous: {}", current.version_string);
    println!("  Current:  {}", new_version.version_string);
    println!();
    println!("{}⚠ Reboot required to load restored firmware.{}", YELLOW, NC);
    println!();

    if confirm("Reboot now?")? {
        println!("Rebooting...");
        std::process::Command::new("reboot")
            .status()
            .context("Failed to reboot")?;
    }

    Ok(())
}

/// Check if there's enough disk space
fn check_disk_space(path: &std::path::Path, required_bytes: u64) -> Result<()> {
    use std::process::Command;

    // Use df to get available space
    let output = Command::new("df")
        .args(["--output=avail", "-B1", path.to_str().unwrap_or("/lib/firmware")])
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let available: u64 = stdout.lines()
        .nth(1)  // Skip header
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    if available < required_bytes {
        bail!(
            "Insufficient disk space. Need {} MB, have {} MB.",
            required_bytes / (1024 * 1024),
            available / (1024 * 1024)
        );
    }

    Ok(())
}

/// Prompt for yes/no confirmation
fn confirm(prompt: &str) -> Result<bool> {
    use std::io::{self, Write};

    print!("{} [y/N] ", prompt);
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    Ok(input.trim().eq_ignore_ascii_case("y") || input.trim().eq_ignore_ascii_case("yes"))
}
