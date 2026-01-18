# hifi-wifi

**Stop WiFi lag on your Steam Deck and Linux gaming devices.**

hifi-wifi automatically optimizes your network for gaming - no configuration needed. It eliminates bufferbloat, reduces latency spikes, and keeps your connection smooth during game streaming and online play.

---

## What It Does

- **Eliminates stuttering** during online gaming and game streaming
- **Picks the fastest WiFi** automatically (prefers 5GHz/6GHz over 2.4GHz)
- **Reduces lag** with intelligent traffic shaping (CAKE qdisc)
- **Saves battery** on Steam Deck while maintaining performance
- **Survives updates** on SteamOS - install once, keep forever
- **Self-healing** - automatically recovers after sleep, roaming, or system updates

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
| `sudo hifi-wifi monitor` | Watch live activity (Ctrl+C to exit) |
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

hifi-wifi uses the CAKE traffic shaper to manage network congestion, monitors your connection quality, and adjusts settings in real-time. It detects WiFi reconnections, roaming events, and power state changes to keep optimizations current.

**[Read the full architecture documentation →](ARCHITECTURE.md)**

---

