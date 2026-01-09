mod network;
mod system;
mod config;
mod utils;

use anyhow::Result;
use clap::{Parser, Subcommand};
use log::{info, error, warn};

use crate::config::loader::load_config;
use crate::network::wifi::{WifiManager, WifiInterface};
use crate::network::backend_tuner::BackendTuner;
use crate::network::governor::Governor;
use crate::system::power::PowerManager;
use crate::system::optimizer::SystemOptimizer;

#[derive(Parser)]
#[command(name = "hifi-wifi")]
#[command(version = "3.0.0")]
#[command(about = "High Fidelity WiFi optimizer for Linux Streaming Handhelds", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    
    /// Run without making changes (show what would be done)
    #[arg(long, global = true)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Apply Wi-Fi optimizations once (default)
    Apply,
    /// Run as daemon with continuous monitoring
    Monitor,
    /// Revert all optimizations to defaults
    Revert,
    /// Show current Wi-Fi status and detected hardware
    Status,
    /// Install system service for automatic optimization
    Install,
    /// Uninstall system service
    Uninstall,
    /// Stop service and revert optimizations (for A/B testing)
    Off,
    /// Start service and apply optimizations (for A/B testing)
    On,
}

#[tokio::main]
async fn main() -> Result<()> {
    utils::logger::init();
    
    let cli = Cli::parse();

    // Root check (except for status command)
    if !matches!(cli.command, Some(Commands::Status)) && !utils::privilege::is_root() {
        error!("This application must be run as root.");
        error!("Try: sudo hifi-wifi");
        std::process::exit(1);
    }

    let config = load_config();

    match cli.command.unwrap_or(Commands::Apply) {
        Commands::Apply => {
            if cli.dry_run {
                info!("[DRY-RUN] Would apply the following optimizations:");
                run_dry_run()?;
            } else {
                run_apply(&config)?;
            }
        }
        Commands::Monitor => {
            run_monitor(&config).await?;
        }
        Commands::Revert => {
            run_revert()?;
        }
        Commands::Status => {
            run_status_async().await?;
        }
        Commands::Install => {
            run_install()?;
        }
        Commands::Uninstall => {
            run_uninstall()?;
        }
        Commands::Off => {
            run_off()?;
        }
        Commands::On => {
            run_on()?;
        }
    }

    Ok(())
}

fn run_apply(config: &config::structs::Config) -> Result<()> {
    info!("=== hifi-wifi v3.0 ===");
    info!("Applying Wi-Fi optimizations...\n");

    // 1. Detect Wi-Fi interfaces
    let wifi_mgr = WifiManager::new()?;
    let interfaces = wifi_mgr.interfaces();
    
    if interfaces.is_empty() {
        error!("No Wi-Fi interfaces detected!");
        return Ok(());
    }

    for ifc in interfaces {
        info!("Found: {} (driver: {}, category: {:?})", 
              ifc.name, ifc.driver, ifc.category);
    }

    // 2. Detect power state
    let power_mgr = PowerManager::new();
    info!("Device type: {:?}", power_mgr.device_type());
    info!("Power source: {:?}", power_mgr.power_source());

    // 3. Apply system optimizations
    if config.system.sysctl_enabled || config.system.driver_tweaks_enabled || config.system.irq_affinity_enabled {
        let sys_opt = SystemOptimizer::new(
            config.system.sysctl_enabled,
            config.system.irq_affinity_enabled,
            config.system.driver_tweaks_enabled,
        );
        
        // Only optimize connected/active interfaces
        let active_interfaces: Vec<WifiInterface> = interfaces
            .iter()
            .filter(|ifc| wifi_mgr.is_interface_connected(ifc))
            .cloned()
            .collect();
        
        if active_interfaces.is_empty() {
            warn!("No active network connections - skipping IRQ optimizations");
        } else {
            info!("Optimizing {} active interface(s)", active_interfaces.len());
            sys_opt.apply(&active_interfaces)?;
        }
    }

    // 4. Apply power-aware settings
    for ifc in interfaces {
        // Skip disconnected interfaces
        if !wifi_mgr.is_interface_connected(ifc) {
            info!("Skipping {} (not connected)", ifc.name);
            continue;
        }
        
        info!("Optimizing connected interface: {}", ifc.name);
        let should_save = match config.power.wlan_power_save.as_str() {
            "on" => {
                info!("Power save forced ON by config on {}", ifc.name);
                true
            },
            "off" => {
                info!("Power save forced OFF by config on {}", ifc.name);
                false
            },
            _ => { // adaptive
                let adaptive = power_mgr.should_enable_power_save();
                if adaptive {
                    info!("On battery - enabling power save on {}", ifc.name);
                } else {
                    info!("On AC/Desktop - disabling power save on {}", ifc.name);
                }
                adaptive
            }
        };

        if should_save {
            wifi_mgr.enable_power_save(ifc)?;
        } else {
            wifi_mgr.disable_power_save(ifc)?;
        }

        // 5. Get link stats and apply CAKE
        // Always apply CAKE, even if we can't get link stats
        let bandwidth = match wifi_mgr.get_link_stats(ifc) {
            Ok(stats) if stats.tx_bitrate_mbps > 0.0 => {
                info!("Link: {}Mbps TX, {}dBm signal", stats.tx_bitrate_mbps, stats.signal_dbm);
                // Use 60% of link rate for realistic Wi-Fi throughput
                (stats.tx_bitrate_mbps * 0.60) as u32
            }
            Ok(stats) => {
                warn!("Link stats returned 0 bitrate (signal: {}dBm), using 200Mbit default", stats.signal_dbm);
                200
            }
            Err(e) => {
                warn!("Failed to get link stats: {}, using 200Mbit default", e);
                200
            }
        };
        
        if let Err(e) = wifi_mgr.apply_cake(ifc, bandwidth.max(1)) {
            error!("Failed to apply CAKE on {}: {}", ifc.name, e);
        }
    }

    // 6. Apply backend tuning
    if config.backend.iwd_periodic_scan_disable {
        let backend_tuner = BackendTuner::new(true);
        backend_tuner.apply()?;
    }

    info!("\n=== Optimization Complete ===");
    Ok(())
}

fn run_dry_run() -> Result<()> {
    let wifi_mgr = WifiManager::new()?;
    let power_mgr = PowerManager::new();
    
    info!("  - Detected {} Wi-Fi interface(s)", wifi_mgr.interfaces().len());
    for ifc in wifi_mgr.interfaces() {
        info!("    * {} ({:?})", ifc.name, ifc.category);
    }
    
    info!("  - Device type: {:?}", power_mgr.device_type());
    info!("  - Power source: {:?}", power_mgr.power_source());
    
    if power_mgr.should_enable_power_save() {
        info!("  - Would ENABLE power save (on battery)");
    } else {
        info!("  - Would DISABLE power save (performance mode)");
    }
    
    info!("  - Would create /etc/sysctl.d/99-hifi-wifi.conf");
    info!("  - Would create driver-specific modprobe config");
    info!("  - Would apply CAKE qdisc for bufferbloat mitigation");
    info!("  - Would optimize IRQ affinity");
    
    Ok(())
}

fn run_revert() -> Result<()> {
    info!("=== Reverting hifi-wifi Optimizations ===\n");

    let wifi_mgr = WifiManager::new()?;
    
    // Remove CAKE qdiscs
    for ifc in wifi_mgr.interfaces() {
        info!("Removing CAKE from {}", ifc.name);
        wifi_mgr.remove_cake(ifc)?;
        
        // Re-enable power save (safe default)
        wifi_mgr.enable_power_save(ifc)?;
    }

    // Revert system optimizations
    let sys_opt = SystemOptimizer::default();
    sys_opt.revert()?;

    // Revert backend tuning
    let backend_tuner = BackendTuner::default();
    backend_tuner.revert()?;

    info!("\n=== Revert Complete ===");
    Ok(())
}

/// Check if we're running on SteamOS
fn is_steamos() -> bool {
    if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
        content.contains("ID=steamos")
    } else {
        false
    }
}

/// Self-healing routine to ensure the binary is accessible via CLI
/// On SteamOS, this handles read-only filesystem after system updates
fn ensure_symlinks() {
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::process::Command;
    use log::{info, warn, debug};

    let target = "/var/lib/hifi-wifi/hifi-wifi";
    // Prefer /usr/local/bin as it's often writable/overlayed on Bazzite/Silverblue
    let link_path = "/usr/local/bin/hifi-wifi";

    if !Path::new(target).exists() {
        warn!("Source binary not found at {}, skipping symlink creation", target);
        return;
    }

    // Check if symlink already exists and is correct
    if Path::new(link_path).exists() {
        if let Ok(existing_target) = std::fs::read_link(link_path) {
            if existing_target.to_string_lossy() == target {
                debug!("Symlink already exists and is correct");
                return;
            }
        }
        // Symlink exists but points somewhere else - remove it
        let _ = std::fs::remove_file(link_path);
    }

    info!("Self-Healing: Creating symlink {} -> {}", link_path, target);
    
    // On SteamOS, we need to disable read-only filesystem first
    let steamos = is_steamos();
    if steamos {
        debug!("SteamOS detected, disabling read-only filesystem for symlink");
        // Unmerge system extensions first
        let _ = Command::new("systemd-sysext")
            .arg("unmerge")
            .output();
        
        // Disable read-only
        let _ = Command::new("steamos-readonly")
            .arg("disable")
            .output();
        
        // Small delay for filesystem to settle
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // Try to create symlink
    if let Err(e) = symlink(target, link_path) {
        warn!("Failed to create symlink at {}: {}", link_path, e);
    } else {
        info!("Symlink created successfully.");
    }

    // Re-enable read-only on SteamOS
    if steamos {
        debug!("Re-enabling SteamOS read-only filesystem");
        let _ = Command::new("steamos-readonly")
            .arg("enable")
            .output();
    }
}

/// Run the Governor in monitor mode (daemon)
async fn run_monitor(config: &config::structs::Config) -> Result<()> {
    info!("=== hifi-wifi v3.0 Monitor Mode ===");
    // Crucial for immutable distros (Bazzite/Silverblue) where /usr/bin is read-only
    ensure_symlinks();

    info!("Starting continuous optimization daemon...\n");

    // Apply initial optimizations
    run_apply(config)?;

    // Start the Governor
    let mut governor = Governor::new(config.governor.clone(), config.wifi.clone()).await?;
    
    info!("Governor initialized, entering main loop (tick: {}s)", 
          config.global.tick_rate_secs);
    
    // Handle graceful shutdown
    let ctrl_c = tokio::signal::ctrl_c();
    
    tokio::select! {
        result = governor.run(config.global.tick_rate_secs) => {
            if let Err(e) = result {
                error!("Governor error: {}", e);
            }
        }
        _ = ctrl_c => {
            info!("\nReceived shutdown signal");
            governor.stop();
        }
    }

    info!("Monitor mode stopped");
    Ok(())
}

/// Run status with async NetworkManager info
async fn run_status_async() -> Result<()> {
    use crate::network::nm::NmClient;
    use std::process::Command;

    // ANSI Colors
    const RED: &str = "\x1b[0;31m";
    const GREEN: &str = "\x1b[0;32m";
    const YELLOW: &str = "\x1b[0;33m";
    const BLUE: &str = "\x1b[0;34m";
    const CYAN: &str = "\x1b[0;36m";
    const BOLD: &str = "\x1b[1m";
    const DIM: &str = "\x1b[2m";
    const NC: &str = "\x1b[0m";

    println!();
    println!("{}{}{}", BOLD, CYAN, "══════════════════════════════════════");
    println!("       hifi-wifi v3.0 Status");
    println!("{}{}{}", BOLD, CYAN, "══════════════════════════════════════");
    println!();

    // 1. Service Status
    let service_active = Command::new("systemctl")
        .args(["is-active", "--quiet", "hifi-wifi.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if service_active {
        println!("{}Status:{}      {}[ACTIVE]{}", BOLD, NC, GREEN, NC);
    } else {
        println!("{}Status:{}      {}[INACTIVE]{}", BOLD, NC, RED, NC);
    }
    println!();

    // 2. System and Power
    let power_mgr = PowerManager::new();
    println!("{}{}{}┌─ System Info{}", BOLD, BLUE, NC, NC);
    println!("{}│{}  Device: {:?}", BLUE, NC, power_mgr.device_type());
    let bat_pct = power_mgr.battery_percentage().map(|p| format!("{}%", p)).unwrap_or("N/A".to_string());
    println!("{}│{}  Power:  {:?} (Battery: {})", BLUE, NC, power_mgr.power_source(), bat_pct);
    println!("{}└{}", BLUE, NC);
    println!();

    // 3. Interfaces & Tweaks (CAKE, Power Save)
    let wifi_mgr = WifiManager::new()?;
    println!("{}{}{}┌─ Interfaces & Tweaks{}", BOLD, BLUE, NC, NC);
    
    if wifi_mgr.interfaces().is_empty() {
         println!("{}│{}  {}No network interfaces detected{}", BLUE, NC, DIM, NC);
    }

    for ifc in wifi_mgr.interfaces() {
        let ifc_type = match ifc.interface_type {
            crate::network::wifi::InterfaceType::Wifi => "WiFi",
            crate::network::wifi::InterfaceType::Ethernet => "Ethernet",
        };
        println!("{}│{}  {}{}{} (Type: {}, Driver: {}, {:?})", BLUE, NC, BOLD, ifc.name, NC, ifc_type, ifc.driver, ifc.category);

        // CAKE Status (tc)
        let qdisc_out = Command::new("tc")
            .args(["qdisc", "show", "dev", &ifc.name])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        
        if qdisc_out.contains("cake") {
             // Extract bandwidth if possible
             let bw = qdisc_out.split("bandwidth ").nth(1)
                .and_then(|s| s.split_whitespace().next())
                .unwrap_or("unknown");
             println!("{}│{}    ├─ CAKE:       {}[ACTIVE]{} Bandwidth: {}", BLUE, NC, GREEN, NC, bw);
        } else {
             println!("{}│{}    ├─ CAKE:       {}[INACTIVE]{}", BLUE, NC, RED, NC);
        }

        // Power Save (iw) - WiFi only
        if ifc.interface_type == crate::network::wifi::InterfaceType::Wifi {
            let ps_out = Command::new("iw")
                .args(["dev", &ifc.name, "get", "power_save"])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();
            
            let ps_status = if ps_out.contains("on") {
                 format!("{}[ON]{} (Power Saving)", YELLOW, NC)
            } else {
                 format!("{}[OFF]{} (Performance)", GREEN, NC)
            };
            println!("{}│{}    ├─ Power Save: {}", BLUE, NC, ps_status);
        } else {
            // For ethernet, show EEE status instead
            let eee_out = Command::new("ethtool")
                .args(["--show-eee", &ifc.name])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();
            
            let eee_status = if eee_out.contains("EEE status: disabled") {
                format!("{}[DISABLED]{} (Low Latency)", GREEN, NC)
            } else if eee_out.contains("EEE status: enabled") {
                format!("{}[ENABLED]{} (Power Saving)", YELLOW, NC)
            } else if eee_out.contains("not supported") || eee_out.contains("Operation not supported") {
                format!("{}[N/A]{} (Not Supported)", DIM, NC)
            } else {
                format!("{}[UNKNOWN]{}", DIM, NC)
            };
            println!("{}│{}    ├─ EEE:        {}", BLUE, NC, eee_status);
        }

        // IRQ Affinity
        let irq_out = std::fs::read_to_string("/proc/interrupts").unwrap_or_default();
        
        // USB devices don't have dedicated IRQs we can pin easily
        let is_usb = ifc.driver.contains("usb") || ifc.name.contains("usb") || ifc.driver.starts_with("rt2800usb");

        let irq_status = if is_usb {
             format!("{}[N/A]{} (USB Device)", DIM, NC)
        } else {
            // Special mappings for drivers that report different names in /proc/interrupts
            // - rtl8192ee reports as "rtl_pci"
            // - ath11k uses MSI-X with multiple IRQ vectors
            // - Steam Deck OLED (WCN6855) may show as wcn, ath11k, MHI, or other variants
            let search_terms: Vec<&str> = match ifc.driver.as_str() {
                "rtl8192ee" => vec!["rtl_pci"],
                "ath11k_pci" | "ath11k" => vec!["ath11k", "wcn", "wlan0", "MHI"],
                _ => vec![ifc.driver.as_str()],
            };

            // Find ALL matching IRQs
            let irq_lines: Vec<&str> = irq_out.lines()
                .filter(|l| search_terms.iter().any(|t| l.contains(t)) || l.contains(&ifc.name))
                .collect();
            
            if !irq_lines.is_empty() {
                 // Check if ALL IRQs are pinned to CPU1
                 let mut all_optimized = true;
                 let mut all_found = true;
                 let mut total = 0;
                 let mut optimized = 0;
                 
                 for line in &irq_lines {
                     let irq_num = line.trim().split(':').next().unwrap_or("?");
                     if let Ok(affinity) = std::fs::read_to_string(format!("/proc/irq/{}/smp_affinity", irq_num)) {
                         total += 1;
                         let aff = affinity.trim();
                         // Check if pinned to CPU1 (mask 0x2 in various formats)
                         let is_cpu1 = aff == "2" || aff == "02" || aff == "00000002" || aff == "000002";
                         if is_cpu1 {
                             optimized += 1;
                         } else {
                             all_optimized = false;
                         }
                     } else {
                         all_found = false;
                     }
                 }
                 
                 if total == 0 || !all_found {
                     format!("{}[UNKNOWN]{}", DIM, NC)
                 } else if all_optimized {
                     if total > 1 {
                         format!("{}[OPTIMIZED]{} (CPU 1, {} vectors)", GREEN, NC, total)
                     } else {
                         format!("{}[OPTIMIZED]{} (CPU 1)", GREEN, NC)
                     }
                 } else {
                     format!("{}[PARTIAL]{} ({}/{} pinned)", YELLOW, NC, optimized, total)
                 }
            } else {
                 format!("{}[NOT FOUND]{}", DIM, NC)
            }
        };
        println!("{}│{}    └─ IRQ Pin:    {}", BLUE, NC, irq_status);
        println!("{}│{}", BLUE, NC);
    }
    println!("{}└{}", BLUE, NC);
    println!();

    // 4. Backend & Governor
    let backend = BackendTuner::default();
    println!("{}{}{}┌─ Network Governor & Backend{}", BOLD, BLUE, NC, NC);
    println!("{}│{}  Backend: {:?}", BLUE, NC, backend.backend());
    
    let config = load_config();
    let gov_status = if service_active { "Running" } else { "Stopped" };
    println!("{}│{}  Governor: {}", BLUE, NC, gov_status);
    println!("{}│{}    ├─ QoS Mode:   {}", BLUE, NC, if config.governor.breathing_cake_enabled { "Breathing CAKE (Dynamic)" } else { "Static CAKE" });
    println!("{}│{}    ├─ Game Mode:  {}", BLUE, NC, if config.governor.game_mode_enabled { "Available (PPS > 200)" } else { "Disabled" });
    println!("{}│{}    └─ Band Steer: {}", BLUE, NC, if config.governor.band_steering_enabled { "Available" } else { "Disabled" });

    println!("{}└{}", BLUE, NC);
    println!();

    // 5. Connection Details (NM)
    if let Ok(nm) = NmClient::new().await {
        println!("{}{}{}┌─ Active Connection (NetworkManager){}", BOLD, BLUE, NC, NC);
        match nm.get_wireless_devices().await {
            Ok(devices) => {
                 let mut found_conn = false;
                 for device in devices {
                     if let Some(ap) = device.active_ap {
                         found_conn = true;
                         println!("{}│{}  {}: {}", BLUE, NC, device.interface, ap.ssid);
                         println!("{}│{}    ├─ BSSID:    {}", BLUE, NC, ap.bssid);
                         println!("{}│{}    ├─ Band:     {:?} ({} MHz)", BLUE, NC, ap.band, ap.frequency);
                         println!("{}│{}    ├─ Signal:   {} dBm", BLUE, NC, ap.signal_strength);
                         println!("{}│{}    └─ Link:     {} Mbit/s", BLUE, NC, device.bitrate / 1000);
                     }
                 }
                 if !found_conn {
                     println!("{}│{}  No active connection found", BLUE, NC);
                 }
            }
            Err(_) => println!("{}│{}  Error querying NetworkManager", BLUE, NC),
        }
        println!("{}└{}", BLUE, NC);
    }
    
    Ok(())
}

/// Install the systemd service
/// Per rewrite.md: Binary in /var/lib/hifi-wifi (survives SteamOS updates)
fn run_install() -> Result<()> {
    use std::fs::{self, File};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    
    info!("=== Installing hifi-wifi Service ===\n");

    // Create persistent directory (survives SteamOS A/B updates)
    let var_lib = std::path::Path::new("/var/lib/hifi-wifi");
    fs::create_dir_all(var_lib)?;
    
    // Copy binary to persistent location
    let current_exe = std::env::current_exe()?;
    let target_bin = var_lib.join("hifi-wifi");
    
    info!("Copying binary to {}", target_bin.display());
    fs::copy(&current_exe, &target_bin)?;
    
    // Make executable
    let mut perms = fs::metadata(&target_bin)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&target_bin, perms)?;

    // Create systemd service
    // Per rewrite.md: Service config with capabilities
    let service_content = r#"[Unit]
Description=hifi-wifi Network Optimizer
Documentation=https://github.com/your-repo/hifi-wifi
After=network-online.target NetworkManager.service
Wants=network-online.target

[Service]
Type=simple
ExecStart=/var/lib/hifi-wifi/hifi-wifi monitor
Restart=on-failure
RestartSec=5

# Security hardening per rewrite.md
ProtectSystem=full
ProtectHome=true
NoNewPrivileges=false
CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN
AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN

# Resource limits
MemoryMax=64M
CPUQuota=10%

[Install]
WantedBy=multi-user.target
"#;

    let service_path = std::path::Path::new("/etc/systemd/system/hifi-wifi.service");
    info!("Creating systemd service: {}", service_path.display());
    
    let mut file = File::create(service_path)?;
    file.write_all(service_content.as_bytes())?;

    // Create systemd preset to survive SteamOS updates
    let preset_dir = std::path::Path::new("/etc/systemd/system-preset");
    if let Err(e) = fs::create_dir_all(preset_dir) {
        warn!("Could not create preset directory: {}", e);
    } else {
        let preset_path = preset_dir.join("99-hifi-wifi.preset");
        if let Ok(mut preset_file) = File::create(&preset_path) {
            let _ = preset_file.write_all(b"enable hifi-wifi.service\n");
            info!("Created systemd preset for automatic enable");
        }
    }

    // Reload systemd and enable service
    info!("Enabling service...");
    Command::new("systemctl").args(["daemon-reload"]).output()?;
    Command::new("systemctl").args(["enable", "hifi-wifi.service"]).output()?;
    Command::new("systemctl").args(["start", "hifi-wifi.service"]).output()?;

    info!("\n=== Installation Complete ===");
    info!("Service installed and started.");
    info!("  Status: systemctl status hifi-wifi");
    info!("  Logs:   journalctl -u hifi-wifi -f");
    
    // Create CLI symlink for user convenience
    ensure_symlinks();
    
    Ok(())
}

/// Uninstall the systemd service
fn run_uninstall() -> Result<()> {
    use std::fs;
    use std::process::Command;
    
    info!("=== Uninstalling hifi-wifi Service ===\n");

    // Stop and disable service
    info!("Stopping service...");
    let _ = Command::new("systemctl").args(["stop", "hifi-wifi.service"]).output();
    let _ = Command::new("systemctl").args(["disable", "hifi-wifi.service"]).output();

    // Remove service file
    let service_path = "/etc/systemd/system/hifi-wifi.service";
    if std::path::Path::new(service_path).exists() {
        info!("Removing service file...");
        fs::remove_file(service_path)?;
    }

    // Reload systemd
    Command::new("systemctl").args(["daemon-reload"]).output()?;

    // Optionally remove binary (keep /var/lib/hifi-wifi for config)
    let binary_path = "/var/lib/hifi-wifi/hifi-wifi";
    if std::path::Path::new(binary_path).exists() {
        info!("Removing binary...");
        fs::remove_file(binary_path)?;
    }

    // Revert optimizations
    run_revert()?;

    info!("\n=== Uninstallation Complete ===");
    Ok(())
}

/// Turn off hifi-wifi (stop service, revert optimizations) for A/B testing
fn run_off() -> Result<()> {
    use std::process::Command;
    
    info!("=== Turning OFF hifi-wifi ===\n");

    // Stop service if running
    if Command::new("systemctl").args(["is-active", "--quiet", "hifi-wifi"]).status()?.success() {
        info!("Stopping hifi-wifi service...");
        Command::new("systemctl").args(["stop", "hifi-wifi.service"]).output()?;
    } else {
        info!("Service not running.");
    }

    // Revert all optimizations
    run_revert()?;

    info!("\n=== hifi-wifi is OFF ===");
    info!("Network is now using default settings.");
    info!("To turn back on: sudo hifi-wifi on");
    Ok(())
}

/// Turn on hifi-wifi (start service, apply optimizations) for A/B testing
fn run_on() -> Result<()> {
    use std::process::Command;
    
    info!("=== Turning ON hifi-wifi ===\n");

    // Check if service exists
    if !std::path::Path::new("/etc/systemd/system/hifi-wifi.service").exists() {
        error!("hifi-wifi service not installed. Run: sudo hifi-wifi install");
        return Ok(());
    }

    // Start service
    info!("Starting hifi-wifi service...");
    Command::new("systemctl").args(["start", "hifi-wifi.service"]).output()?;

    info!("\n=== hifi-wifi is ON ===");
    info!("Network optimizations are active.");
    info!("Check status: hifi-wifi status");
    Ok(())
}
