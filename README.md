> [!IMPORTANT]  
> **Upgrading from v1.x?** Uninstall first: `cd legacy && sudo ./uninstall.sh && cd ..`

---

## Bug Fixes

### WiFi Reconnection Stutters (Issue #10)
**Before:** Games stutter after your Steam Deck wakes from sleep or switches networks. You had to manually reconnect WiFi to fix it.

**Now:** Automatically detects reconnections and clears stale data. Your games stay smooth without manual intervention.

### Service Stability on SteamOS
**Before:** Service crashed after SteamOS updates when trying to write configs to read-only `/etc`.

**Now:** Handles read-only filesystems gracefully. Service keeps running even if some optimizations can't be applied.

### Optimizations After Updates
**Before:** After SteamOS updates, optimizations (sysctl, modprobe) weren't reapplied until you manually restarted.

**Now:** All optimizations automatically restored on first boot after update.

**[View commits →](https://github.com/doughty247/hifi-wifi/compare/4066275...b1a95e4)**

---

## New Features

### Automatic Fast WiFi Selection
Your device now picks the fastest available WiFi connection, not just the strongest signal.

- Prefers 5GHz over 2.4GHz when both are available
- Switches to faster networks more aggressively
- **Experimental:** WiFi 6E (6GHz band) support
- Better game streaming and faster downloads

### SteamOS: Survives System Updates
**Before:** SteamOS updates broke hifi-wifi. You had to reinstall.

**Now:** Persists across updates automatically. Install once, keep forever.

### SteamOS: No More Manual Fixes
Service repairs itself automatically after updates. No more terminal commands to re-enable read-only mode or fix broken installations.

### Easy Developer Setup
Want to help test or contribute? Installer now sets up Homebrew and Rust automatically on SteamOS. Just clone and run `./install.sh` - it handles everything.

(Official releases still provide pre-compiled binaries - no build required for regular users)

### Other Improvements
- CAKE works more consistently - speed tests reach full speeds instead of getting stuck at 50Mbit
- Installer gives clearer feedback if something goes wrong

---

## Testing Checklist

**Please test and report issues!** This helps everyone.

- [ ] Installer completes without errors
- [ ] `hifi-wifi status` works after opening new terminal
- [ ] Service survives reboot
- [ ] WiFi disconnect/reconnect shows cache clearing in logs: `journalctl -u hifi-wifi -f`
- [ ] Dual-band WiFi roams to better band
- [ ] (SteamOS) Service persists after system update

**Report issues:** Attach output of `{ hifi-wifi status; journalctl -u hifi-wifi -n 100; } > report.txt` to [GitHub Issues](https://github.com/doughty247/hifi-wifi/issues)

---

## Commands

| Command | Description |
|---------|-------------|
| `hifi-wifi status` | Show current WiFi state and optimization status |
| `sudo hifi-wifi monitor` | Watch live logs (Ctrl+C to exit) |
| `sudo hifi-wifi on/off` | Start/stop the service |
| `sudo hifi-wifi uninstall` | Remove hifi-wifi completely |
| `journalctl -u hifi-wifi -n 50` | View last 50 log entries |
| `journalctl -u hifi-wifi -f` | Follow logs in real-time |

---

## Supported Platforms

- **SteamOS 3.x** (Steam Deck LCD/OLED)
- **Bazzite** (Well tested)
- **Arch Linux** / **Fedora**

---

## Known Issues

- **Multiple networks**: May not prioritize ethernet over WiFi when both are connected.
- **Homebrew warning**: "post-install step did not complete" - harmless, ignore it
- **Command not found**: Open new terminal after install to load `hifi-wifi` command

---

**Full changelog:** [`4066275...b1a95e4`](https://github.com/doughty247/hifi-wifi/compare/4066275...b1a95e4)
