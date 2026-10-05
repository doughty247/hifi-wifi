# hifi-wifi

**Lightweight, system-wide network optimizer for Linux gaming devices.**

hifi-wifi automatically optimizes your network for low latency, eliminating bufferbloat and reducing packet loss during online gaming and high-bitrate game streaming (Moonlight/Sunshine). It runs as a native background service requiring zero configuration.

---

## Key Features

* **Autorate Bufferbloat Control**: Measures latency while your link is busy and keeps the CAKE shaper just under the real bottleneck (your ISP line or the Wi-Fi link), so downloads and uploads stop wrecking game and stream latency. No speeds to configure; learned rates are remembered per network.
* **Game Traffic Priority**: Steam, Remote Play and Moonlight/Sunshine UDP is marked DSCP EF, so it uses the Wi-Fi voice/video queue and CAKE's latency tin ahead of bulk uploads.
* **Proof, Not Promises**: `hifi-wifi bench --ab` measures latency under load with hifi-wifi off and on, back to back. `hifi-wifi doctor` tells you what limits your connection (signal, band, retries, channel congestion) and how to fix it.
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
| `hifi-wifi doctor` | Diagnose your Wi-Fi: signal, band, retransmissions, channel busy time, power save |
| `sudo hifi-wifi bench` | Measure latency under load (bufferbloat grade) |
| `sudo hifi-wifi bench --ab` | Same, with hifi-wifi off then on, and a side-by-side comparison |
| `sudo hifi-wifi autorate` | Watch autorate work in the foreground (stop the service first) |
| `sudo hifi-wifi check-compat` | Check that tc, CAKE, IFB, ping, nft and curl are available |
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

WiFi drivers perform background channel scans every ~15 seconds, causing **170ms latency spikes** that affect gaming and streaming. 

By default, `hifi-wifi` uses a smart, **Adaptive** scanning governor that gets the best of both worlds:
1. **Gaming/Streaming Active:** Background scanning is completely suppressed to guarantee a pristine 3-4ms connection.
2. **System Wake/Reboot:** Scanning is temporarily allowed for 30 seconds to ensure the device reconnects to the network instantly and seamlessly.
3. **Signal Degraded:** If you walk away from your AP and the signal drops below the usable threshold, scanning is automatically allowed so the device can steer/roam to a stronger access point.
4. **Strong Signal (Idle):** Scans are suppressed to avoid random spikes while browsing or watching media.

You can customize this behavior at any time:

To force background scanning always **OFF** (suppressed — lowest latency, disables roaming):
```bash
sudo hifi-wifi scan off
```

To force background scanning always **ON** (allowed — roaming enabled):
```bash
sudo hifi-wifi scan on
```

To restore the default smart **ADAPTIVE** governor:
```bash
sudo hifi-wifi scan adaptive
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

Latency spikes under load ("bufferbloat") happen when a queue somewhere fills up, usually in your modem or router. hifi-wifi moves that queue onto your device, where CAKE keeps it short and fair. **Autorate** decides the shaper rate: it pings a few public resolvers, and when latency rises while this device is saturating the link, it lowers the rate to just under the bottleneck; while latency stays clean it raises it again. It only shapes while the link is busy, so idle use costs nothing.

It also disables Wi-Fi power save when it hurts latency, suppresses background scans during play, steers to 5/6 GHz, and marks game traffic for the Wi-Fi voice/video queue. It detects reconnections, roaming and power changes to keep this current.

What it cannot fix: weak signal, a congested channel, or other devices saturating your network. `hifi-wifi doctor` tells you when that is the problem.

**[Read the full architecture documentation →](docs/ARCHITECTURE.md)**

---

