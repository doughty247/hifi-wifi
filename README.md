# hifi-wifi

hifi-wifi is a network optimization tool for Linux systems, specifically targeting SteamOS and Bazzite. It configures the CAKE queue discipline to mitigate bufferbloat and manages Wi-Fi power save modes based on power source.

## Features

*   **Bufferbloat Mitigation**: Applies CAKE queue discipline with automatic bandwidth detection (85% for Wi-Fi, 95% for Ethernet).
*   **Network Profiling**: Detects and profiles networks individually. Settings are saved and reapplied on reconnection.
*   **Power Management**: Automatically switches between performance mode (AC) and power-saving mode (Battery) on supported devices.
*   **Hardware Support**: Optimizes settings for Realtek, MediaTek, Intel, Atheros, Broadcom, and Marvell chipsets.
*   **Backend Switching**: Automatically switches to `iwd` (iNet Wireless Daemon) for improved roaming and connection speed on supported systems.

## System Requirements

*   Linux kernel 5.15 or newer
*   NetworkManager
*   Root access (sudo)
*   CAKE qdisc support (`sch_cake` module)
*   Wi-Fi adapter

## Installation

Clone the repository and run the installer:

```bash
git clone https://github.com/doughty247/hifi-wifi.git
cd hifi-wifi
chmod +x install.sh
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
| `--list-networks` | List saved network profiles. |
| `--view-log` | View auto-optimization log. |
| `--interface <IFACE>` | Specify Wi-Fi interface (auto-detect if omitted). |
| `--no-diagnose` | Skip diagnostic sections (only apply/revert). |
| `--no-iwd` | Do not switch backend to `iwd` (keep `wpa_supplicant`). |
| `--force-performance` | Disable power-saving even on battery (prevents jitter). |
| `--dry-run` | Show what would be changed without making changes. |
| `--quiet` | Minimal output. |
| `--no-color` | Disable colored output. |

## Configuration

### Network Profiling

When connecting to a network, the tool:
1.  Checks for an existing profile in `/var/lib/wifi_patch/networks/`.
2.  If no profile exists, it monitors network activity.
3.  If the network is idle, it detects link speed and creates a profile with a bandwidth limit:
    *   **Wi-Fi**: 85% of detected speed (for stability).
    *   **Ethernet**: 95% of detected speed (for performance).
4.  Applies CAKE qdisc with the calculated limit.

### Power Management

*   **Desktop**: Power saving is always disabled.
*   **Battery Devices**:
    *   **AC**: Power saving disabled (Performance mode).
    *   **Battery**: Power saving enabled.
    *   **Forced Performance**: Use `--force-performance` to disable power saving on battery.

### Wi-Fi Backend (iwd)

On Bazzite and SteamOS, the script defaults to using `iwd` as the Wi-Fi backend for NetworkManager. This provides faster connection times and better roaming.

To opt-out and stay on `wpa_supplicant`:

```bash
sudo hifi-wifi --apply --no-iwd
```

## Supported Systems

*   **Primary Targets**: Bazzite, SteamOS (Steam Deck LCD & OLED)
*   **Compatible**: Fedora, Debian, Ubuntu, Arch Linux (and most NetworkManager-based distros)

## Reporting Issues

Please report issues to the issue tracker. Include the output of:

```bash
uname -a
lspci | grep -i network
sudo hifi-wifi --status
```
---
*Not affiliated with Valve Corporation. But that would be dope.*
