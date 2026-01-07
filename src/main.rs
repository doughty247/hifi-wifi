mod network;
mod system;
mod config;
mod utils;

use anyhow::Result;
use clap::{Parser, Subcommand};
use log::{info, error};

use crate::config::loader::load_config;
use crate::network::wifi::WifiManager;
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
        sys_opt.apply(interfaces)?;
    }

    // 4. Apply power-aware settings
    for ifc in interfaces {
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
        if let Ok(stats) = wifi_mgr.get_link_stats(ifc) {
            let bandwidth = if stats.tx_bitrate_mbps > 0.0 {
                // Use 60% of link rate for realistic Wi-Fi throughput
                (stats.tx_bitrate_mbps * 0.60) as u32
            } else {
                200 // Default fallback
            };
            
            info!("Link: {}Mbps TX, {}dBm signal", stats.tx_bitrate_mbps, stats.signal_dbm);
            wifi_mgr.apply_cake(ifc, bandwidth.max(1))?;
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

/// Self-healing routine to ensure the binary is accessible via CLI
fn ensure_symlinks() {
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use log::{info, warn};

    let target = "/var/lib/hifi-wifi/hifi-wifi";
    // Prefer /usr/local/bin as it's often writable/overlayed on Bazzite/Silverblue
    let link_path = "/usr/local/bin/hifi-wifi";

    if !Path::new(target).exists() {
        warn!("Source binary not found at {}, skipping symlink creation", target);
        return;
    }

    if !Path::new(link_path).exists() {
        info!("Self-Healing: Creating symlink {} -> {}", link_path, target);
        if let Err(e) = symlink(target, link_path) {
            warn!("Failed to create symlink at {}: {}", link_path, e);
        } else {
            info!("Symlink created successfully.");
        }
    } else {
        // Optional: Check if it points to the right place and fix if broken?
        // For now, if it exists, assume it's fine.
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
         println!("{}│{}  {}No Wi-Fi interfaces detected{}", BLUE, NC, DIM, NC);
    }

    for ifc in wifi_mgr.interfaces() {
        println!("{}│{}  {}{}{} (Driver: {}, {:?})", BLUE, NC, BOLD, ifc.name, NC, ifc.driver, ifc.category);

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

        // Power Save (iw)
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

        // IRQ Affinity
        let irq_out = std::fs::read_to_string("/proc/interrupts").unwrap_or_default();
        let irq_line = irq_out.lines().find(|l| l.contains(&ifc.driver) || l.contains(&ifc.name));
        let irq_status = if let Some(line) = irq_line {
             let irq_num = line.trim().split(':').next().unwrap_or("?");
             // Check affinity (try hex or decimal)
             if let Ok(affinity) = std::fs::read_to_string(format!("/proc/irq/{}/smp_affinity", irq_num)) {
                 let aff = affinity.trim();
                 if aff == "2" || aff == "00000002" || aff == "02" {
                     format!("{}[OPTIMIZED]{} (CPU 1)", GREEN, NC)
                 } else {
                     format!("{}[DEFAULT]{} (Mask: {})", YELLOW, NC, aff)
                 }
             } else {
                 format!("{}[UNKNOWN]{}", DIM, NC)
             }
        } else {
             format!("{}[NOT FOUND]{}", DIM, NC)
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
    println!("{}│{}    ├─ Game Mode:  {}", BLUE, NC, if config.governor.game_mode_enabled { "Enabled (PPS Detection)" } else { "Disabled" });
    println!("{}│{}    └─ Band Steer: {}", BLUE, NC, if config.governor.band_steering_enabled { "Enabled" } else { "Disabled" });

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
