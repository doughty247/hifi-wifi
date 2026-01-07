# hifi-wifi (Legacy v1.x)

> **⚠️ NOTE: MAINTENANCE MODE / LEGACY**
> This branch (`main`) contains the legacy shell-script version (v1.x) of hifi-wifi.
> **A complete rewrite (v3.0) in Rust is currently in active development on the [`dev`](https://github.com/doughty247/hifi-wifi/tree/dev) branch.**
> The `dev` branch is currently unstable and intended for developers only. Please stick to `main` for stable use.

hifi-wifi is a network optimization tool specifically targeting SteamOS and Bazzite. It configures the CAKE queue discipline to mitigate bufferbloat and manages Wi-Fi power save modes based on power source.

## Features

*   **Bufferbloat Mitigation**: Applies CAKE queue discipline with **Instant Link Statistics** (replaces slow speedtests) to dynamically adjust bandwidth limits.
*   **Multi-Interface Support**: Optimizes ALL active network interfaces (Ethernet + Wi-Fi) simultaneously.
*   **Power Management**: Automatically switches between performance mode (AC) and power-saving mode (Battery) on supported devices.
*   **Hardware Support**: Optimizes settings for Realtek, MediaTek, Intel, Atheros, Broadcom, and Marvell chipsets.
*   **iwd Optimization**: If iwd is active on your system, applies optimized roaming and scanning settings.

## System Requirements

*   Linux kernel 5.15 or newer
*   NetworkManager
*   Root access (sudo)
*   CAKE qdisc support (`sch_cake` module)
*   Wi-Fi adapter

## Installation

### SteamOS Users
If you are running SteamOS (Steam Deck), you may need to initialize the pacman keyring before installation if you haven't done so previously:

```bash
sudo pacman-key --init
sudo pacman-key --populate holo
```

### General Installation

Clone the repository and install the latest release candidate (v1.3.0-rc2) from the testing branch:

```bash
git clone https://github.com/doughty247/hifi-wifi.git
cd hifi-wifi
git checkout testing
sudo ./install.sh
```

## Usage

Apply optimizations:

```bash
sudo hifi-wifi --apply
```

Revert all changes:

```bash
sudo hifi-wifi --revert
```

Check status:

```bash
sudo hifi-wifi
sudo hifi-wifi --status
```

### Options

| Option | Description |
| :--- | :--- |
| `--apply` | Apply performance and stability patches. |
| `--revert` | Revert previously applied patches. |
| `--status` | Show current patch status. |
| `--list-backups` | List available connection backups. |
| `--view-log` | View auto-optimization log. |
| `--interface <IFACE>` | Specify Wi-Fi interface (auto-detect if omitted). |
| `--no-diagnose` | Skip diagnostic sections (only apply/revert). |
| `--force-performance` | Disable power-saving even on battery (prevents jitter). |
| `--dry-run` | Show what would be changed without making changes. |
| `--quiet` | Minimal output. |
| `--no-color` | Disable colored output. |

## Configuration

### Network Profiling

WhenInstant Link Statistics

hifi-wifi uses **Instant Link Statistics** to determine the optimal bandwidth limit for CAKE:
1.  **Instant**: Link speed is detected in milliseconds using `iw` (Wi-Fi) or `ethtool` (Ethernet).
2.  **Dynamic**: Adapts to your current link quality (e.g., moving closer/farther from the router).
3.  **Smart Limits**:
    *   **Wi-Fi**: 85% of link speed (for stability).
    *   **Ethernet**: 95% of link speed (for performance).
    *   **Minimum**: 1 Mbit floor to prevent connectivity loss

*   **Desktop**: Power saving is always disabled.
*   **Battery Devices**:
    *   **AC**: Power saving disabled (Performance mode).
    *   **Battery**: Power saving enabled.
    *   **Forced Performance**: Use `--force-performance` to disable power saving on battery.

### Wi-Fi Backend

hifi-wifi does **not** change your WiFi backend. Use your OS tools instead:

*   **SteamOS**: Developer Options → "Force WPA Supplicant WiFi backend" (iwd is default)
*   **Bazzite**: Run `ujust toggle-iwd` to switch between wpa_supplicant and iwd

If iwd is already active on your system, hifi-wifi will automatically apply iwd-specific optimizations.

## Supported Systems

Bazzite, SteamOS (Steam Deck LCD & OLED)

## Reporting Issues

Please report issues to the issue tracker. Include the output of:

```bash
uname -a
lspci | grep -i network
sudo hifi-wifi --status
```
---
*Not affiliated with Valve Corporation. But that would be dope.*
