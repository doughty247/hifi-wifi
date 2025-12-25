hifi-wifi v1.2.0 - Merry Christmas Update

Critical stability release addressing SteamOS installation hangs, reboot issues, and network reconnection failures.

Bug Fixes:

**Installation Stability**
* Fixed install.sh hanging indefinitely on SteamOS when reverting old patches
* Added strict timeouts to all `nmcli` commands to prevent freezes
* Installer now uses local (fixed) binary for cleanup instead of potentially broken installed version

**Reboot Hang Fix**
* Fixed system reboot hanging after hifi-wifi installation
* Root cause: dispatcher script (`99-wifi-auto-optimize`) was calling `nmcli` without timeouts during shutdown
* Added 2-second timeouts to dispatcher script network calls

**Network Reconnection**
* Fixed Wi-Fi not auto-reconnecting after backend switch on SteamOS
* Explicitly calls `nmcli connection up` after radio toggle (SteamOS doesn't auto-reconnect)
* Added robust connectivity verification (ping 8.8.8.8) before proceeding
* Increased wait timeout from 30s to 45s with progress updates

**Safe Backend Revert**
* `--revert` now checks if hifi-wifi actually changed the backend before reverting
* Prevents accidentally breaking systems where `iwd` is the native default (recent SteamOS versions)
* Only removes `/etc/NetworkManager/conf.d/wifi_backend.conf` if it exists

**Ethernet Support**
* Added comprehensive ethernet detection throughout codebase
* Install, apply, and revert operations now properly handle ethernet connections
* No longer gets stuck waiting for Wi-Fi when connected via ethernet

**Profile Preservation**
* Network connection profiles are now backed up before revert and restored after
* Ensures automatic reconnection to the same network after `--revert`

**Directory Creation Fix**
* Fixed "No such file or directory" errors on fresh installs ([#4](https://github.com/doughty247/hifi-wifi/issues/4))
* State directories (`/var/lib/wifi_patch/networks`) now created during install
* `apply_patches()` also ensures directories exist at runtime

Known Issues:
* **Power Mode Stuck in Performance**: Wi-Fi power mode may remain stuck in performance mode even when on battery. Investigating for future release.

---

hifi-wifi v1.1.0

Major update focusing on persistence, stability, and user control.

New Features:
* **SteamOS Persistence**: hifi-wifi now survives SteamOS system updates! A new restore service automatically reinstalls the tool and dependencies if the system partition is wiped.
* **Force Performance Mode**: Added `--force-performance` flag to permanently disable Wi-Fi power saving, regardless of battery state.
* **Smart Updates**: The installer now safely handles updates by backing up NetworkManager profiles, reverting old patches, and restoring settings automatically.
* **Backend Switching**: Improved reliability when switching to the `iwd` backend, including automatic package caching for offline restoration.

Improvements:
* **Installer**: Added robust handling for SteamOS read-only filesystem (steamos-readonly disable/enable).
* **UX**: Changed default reboot prompt to "Yes" for smoother installation flow.
* **Fixes**: Resolved issues with missing `enable_iwd` command and systemd service typos.

Installation:
git clone https://github.com/doughty247/hifi-wifi.git
cd hifi-wifi
sudo ./install.sh

---

hifi-wifi v1.0.0

Initial release of the hifi-wifi network optimization utility.

This tool addresses Wi-Fi latency and stability issues on Linux handhelds (Steam Deck, Bazzite) by enforcing CAKE queue disciplines and context-aware power management.

Key Features:
* Bufferbloat Mitigation: Configures sch_cake with adaptive bandwidth overhead (85% for Wi-Fi).
* Power Management: Automates transition between performance (AC) and power-saving (Battery) states to prevent jitter.
* Driver Tuning: Applies specific parameters for Realtek and Intel wireless adapters.
* Diagnostics: Integrated self-test suite for signal health and latency analysis.

Installation:
git clone https://github.com/doughty247/hifi-wifi.git
cd hifi-wifi
sudo ./install.sh
