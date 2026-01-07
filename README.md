# hifi-wifi v3.0 (Rust Rewrite)

> **⚠️ EXPERIMENTAL / PROOF OF CONCEPT**
> This branch (`dev`) contains the complete rewrite of hifi-wifi from Bash to Rust. It is currently in active development.
> For the stable shell-script version, please check the `main` branch or the `legacy/` directory.

hifi-wifi is a high-frequency, fault-tolerant network optimizer for Linux gaming handhelds (Steam Deck, Bazzite, ChimeraOS). It eliminates jitter and bufferbloat by dynamically tuning the CAKE queue discipline and managing Wi-Fi power states.

## Why Rust?

v3.0 moves away from the legacy "script wrapper" approach to a proper system daemon:
*   **Stateful Logic**: Maintains history (hysteresis) to prevent constant roaming/shaping resets.
*   **OS Integration**: Uses native DBus/Netlink APIs instead of parsing text from `ip` or `iw`.
*   **Safety**: Zero runtime parsing errors.
*   **Performance**: compiled binary with minimal CPU footprint.

## Core Features

*   **Breathing CAKE**: Dynamically adjusts bandwidth limits based on real-time negotiation speed (NetworkManager) to keep latency under 5ms.
*   **Smart Roaming**: Uses hysteresis (stickiness) to prevent ping-ponging between 2.4GHz and 5GHz bands.
*   **Game Mode**: Detects high-PPS traffic (gaming) and temporarily forces performance modes, disabling interrupt coalescing if CPU load is critical.
*   **Power Awareness**: Automatically toggles Wi-Fi power save based on AC/Battery state (adaptive).

## Installation (Bazzite/SteamOS/Arch)

1.  **Build & Install**:
    ```bash
    git checkout dev
    ./install.sh
    ```
    This will compile the binary using `cargo` (installing it locally if needed) and set up the systemd service.

2.  **Verify Status**:
    ```bash
    hifi-wifi status
    ```
    You should see the new high-fidelity status dashboard.

## Manual Usage

*   `sudo hifi-wifi monitor`: (Default) Runs the daemon.
*   `hifi-wifi status`: Shows connectivity and tuning details.
*   `sudo hifi-wifi apply`: One-shot optimization application.
*   `sudo hifi-wifi revert`: Removes all optimizations and qdiscs.

## Configuration

Configuration is optional! hifi-wifi is zero-config by default.
However, advanced tuning is available in `/etc/hifi-wifi/config.toml` (created on first run if missing).

```toml
[governor]
breathing_cake_enabled = true
game_mode_enabled = true
# ...
```

## Contributing

Pull requests are welcome! Please ensure `cargo build --release` passes without warnings.
