# hifi-wifi

**Lightweight, system-wide network optimizer for Linux gaming devices.**

hifi-wifi automatically optimizes your network for low latency, eliminating bufferbloat and reducing packet loss during online gaming and high-bitrate game streaming (Moonlight/Sunshine). It runs as a native background service requiring zero configuration.

---

## Key Features

* **Low-Overhead Daemon**: Written in native Rust, running as a systemd service with a minimal CPU and memory footprint (<5MB RAM).
* **Real-time Performance Governor**: Dynamically manages traffic shaping queues, schedules CPU coalescing, and pins network IRQs during high-throughput gaming sessions.
* **BBR Congestion Control**: Enforces TCP BBR congestion control globally to maintain high throughput and reduce packet retransmission on wireless links.
* **Direct PCIe & Link Power Override**: Manages PCIe ASPM states and runtime power management directly via sysfs to ensure hardware responsiveness under load.
* **Intelligent Band Steering**: Scores and steers connections to the optimal frequency band (prefers 5GHz/6GHz based on SNR) without losing roaming capabilities.
* **Unified Network Identity**: Persistent options to override system hostname and MAC address randomization directly from a single configuration file.

---

## Installation

### Download & Install (Recommended)

1. Download the latest release from [GitHub Releases](https://github.com/doughty247/hifi-wifi/releases)
2. Extract the archive
3. Open terminal in the extracted folder
4. Run: `sudo ./install.sh`

That's it! The service starts automatically.

### Build from Source (Developers/Testers)

```bash
git clone https://github.com/doughty247/hifi-wifi.git
cd hifi-wifi
sudo ./install.sh
```

On SteamOS, the installer sets up Homebrew and Rust automatically. First build takes ~10 minutes.

---

## Usage

hifi-wifi runs automatically in the background. You don't need to do anything.

### Commands

| Command | Description |
|---------|-------------|
| `hifi-wifi status` | Check if it's working |
| `sudo hifi-wifi power-save off` | Maximum WiFi performance (persists across sleep/reboot) |
| `sudo hifi-wifi power-save adaptive` | Automatic power save based on AC/battery (default) |
| `hifi-wifi power-save status` | Show current power save mode and actual state |
| `sudo hifi-wifi scan on` | Allow background scans (enables roaming) (default) |
| `sudo hifi-wifi scan off` | Suppress background scans for lowest latency |
| `hifi-wifi scan status` | Show current background scanning state |
| `sudo hifi-wifi on/off` | Start/stop the service |
| `sudo hifi-wifi uninstall` | Remove completely |

### Checking Logs

```bash
journalctl -u hifi-wifi -f      # Follow logs in real-time
journalctl -u hifi-wifi -n 50   # Last 50 log entries
```

---

## Supported Platforms

- **Steam Deck** (LCD & OLED) - SteamOS 3.x
- **Bazzite** (Well tested)
- **Arch Linux** / **Fedora** / other systemd distros

Works on any Linux system with NetworkManager and systemd.

---

## Configuration (Optional)

hifi-wifi works great with default settings. Advanced users can customize:

### WiFi Power Save

By default, hifi-wifi uses **adaptive** power save (off on AC, on when on battery). If you experience WiFi issues on Steam Deck (stuttering, disconnects, slow speeds), force it off:

```bash
sudo hifi-wifi power-save off
```

This persists across sleep, reboot, and SteamOS updates. To revert to automatic mode:

```bash
sudo hifi-wifi power-save adaptive
```

### Background Scanning & Roaming

WiFi drivers perform background channel scans every ~15 seconds, causing **170ms latency spikes** that affect gaming and streaming. By default, hifi-wifi now leaves background scanning ON to ensure smooth WiFi roaming between access points.

If you do not need roaming (e.g. single access point, stationary gaming/streaming), you can disable background scanning (suppress scans) to reduce latency to **~3.5ms average / 4ms max**:

```bash
sudo hifi-wifi scan off
```

To re-enable background scanning (and restore roaming):

```bash
sudo hifi-wifi scan on
```

**Config File:** `/etc/hifi-wifi/config.toml` (created on first run)

---

## Upgrading from v1.x

v3.0 is a complete rewrite. Uninstall v1.x first:

```bash
cd legacy && sudo ./uninstall.sh && cd ..
```

Then install normally.

---

## Getting Help

**Something not working?**

1. Check status: `hifi-wifi status`
2. Collect logs: `{ hifi-wifi status; journalctl -u hifi-wifi -n 100; } > report.txt`
3. [Open an issue](https://github.com/doughty247/hifi-wifi/issues) and attach `report.txt`

---

## How It Works

hifi-wifi uses the CAKE traffic shaper to manage network congestion, suppresses latency-causing background WiFi scans, monitors your connection quality, and adjusts settings in real-time. It detects WiFi reconnections, roaming events, and power state changes to keep optimizations current.

**[Read the full architecture documentation →](ARCHITECTURE.md)**

---

