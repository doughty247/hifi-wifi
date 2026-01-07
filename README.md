# hifi-wifi (v3.0)

hifi-wifi is a high-performance network optimization daemon specifically targeting SteamOS and Bazzite. It eliminates bufferbloat, prevents latency spikes, and intelligently manages power settings to ensure a lag-free gaming experience on handhelds.

This version (v3.0) is a complete rewrite in Rust, offering improved stability, lower resource usage, and seamless system integration compared to previous shell-script versions.

## Features

*   **Intelligent Traffic Shaping**: Dynamically applies and adjusts the CAKE queue discipline to eliminate bufferbloat without manual configuration.
*   **Jitter Reduction**: Prioritizes gaming traffic and small packets to maintain consistent low latency.
*   **Adaptive Power Management**:
    *   **AC Power**: Disables Wi-Fi power saving for maximum stability.
    *   **Battery**: Enables power saving to preserve battery life, but automatically switches to performance mode if network lag is detected.
*   **Self-Healing**: Runs as a background daemon that constantly monitors network state, ensuring optimizations persist through connection drops, roaming, and sleep cycles.
*   **Zero Configuration**: Works out of the box with sensible defaults for Steam Deck and similar hardware.

## System Requirements

*   Linux kernel 5.15 or newer (with `sch_cake` module)
*   NetworkManager
*   systemd
*   Root access (sudo)
*   Rust toolchain (cargo) for installation

## Installation

### Quick Install (All Platforms)

```bash
git clone -b dev https://github.com/doughty247/hifi-wifi.git
cd hifi-wifi
sudo ./install.sh
```

**On SteamOS (Steam Deck):** The installer will automatically detect your system and prepare the build environment if needed. This includes:
- Unmerging system extensions
- Disabling read-only filesystem
- Installing build tools (one-time setup)

**On Bazzite/Other Linux:** The installer works directly without additional setup.

### What the Installer Does

The installer will:
1.  Detect your platform (SteamOS, Bazzite, etc.)
2.  Set up build environment if needed (SteamOS only, first run)
3.  Check for the Rust toolchain (installing `rustup` if missing)
4.  Compile the release binary
5.  Install the binary to `/var/lib/hifi-wifi/` (survives updates)
6.  Create a symlink at `/usr/local/bin/hifi-wifi`.
4.  Enable and start the `hifi-wifi` systemd service.

## Usage

Once installed, hifi-wifi runs automatically in the background.

### Managing the Service

Check service status:
```bash
systemctl status hifi-wifi
```

Restart the service:
```bash
sudo systemctl restart hifi-wifi
```

### CLI Commands

Check current optimization status:
```bash
hifi-wifi status
```

Monitor realtime operations (foreground mode):
```bash
sudo hifi-wifi monitor
```

Manually apply optimizations (one-shot):
```bash
sudo hifi-wifi apply
```

Remove all optimizations:
```bash
sudo hifi-wifi revert
```

## Configuration

hifi-wifi operates with optimal defaults, but advanced users can configure specific behaviors.

**Config File**: `/etc/hifi-wifi/config.toml`

The file is generated on first run. You can adjust parameters such as:
*   Traffic shaping interfaces
*   Power management aggressiveness
*   Logging levels

## Supported Systems

*   **Bazzite** (Recommended)
*   **SteamOS** (Steam Deck LCD & OLED)
*   **Arch Linux** (and derivatives)

## Reporting Issues

Please report bugs on the GitHub Issue Tracker. Include the output of `hifi-wifi status` in your report.
