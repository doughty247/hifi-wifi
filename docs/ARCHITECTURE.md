# hifi-wifi Architecture

**A Technical Deep-Dive into Low-Latency Network Optimization for Linux Gaming**

Version 3.0 | January 2026

---

## Table of Contents

1. [Executive Summary](#executive-summary)
2. [Problem Statement](#problem-statement)
3. [Solution Overview](#solution-overview)
4. [System Architecture](#system-architecture)
5. [Core Components](#core-components)
   - [Network Governor](#network-governor)
   - [CAKE Traffic Shaper](#cake-traffic-shaper)
   - [Band Steering](#band-steering)
   - [Power Management](#power-management)
   - [Connection Event Handler](#connection-event-handler)
   - [IRQ Affinity Optimization](#irq-affinity-optimization)
6. [Platform Integration](#platform-integration)
   - [SteamOS Persistence](#steamos-persistence)
   - [NetworkManager Integration](#networkmanager-integration)
   - [Systemd Service](#systemd-service)
7. [Data Flow](#data-flow)
8. [Configuration](#configuration)
9. [Security Model](#security-model)
10. [Performance Characteristics](#performance-characteristics)
11. [Future Roadmap](#future-roadmap)

---

## Executive Summary

hifi-wifi is a network optimization daemon designed specifically for Linux gaming devices, with primary focus on Steam Deck and similar handhelds. It addresses the fundamental disconnect between how consumer WiFi is optimized (throughput, power saving) versus what gaming actually needs (consistent low latency, minimal jitter).

The system operates as a background service that:
- Applies CAKE (Common Applications Kept Enhanced) traffic shaping
- Dynamically adjusts bandwidth allocation based on real-time PHY rates
- Manages WiFi power states based on device context (AC/battery, gaming/idle)
- Handles network transitions (roaming, reconnection, sleep/wake) transparently
- Survives SteamOS's immutable filesystem updates

**Key Results:**
- 60-80% reduction in latency jitter during network congestion
- Automatic recovery from WiFi reconnection issues (Issue #10)
- Zero-configuration operation with sensible defaults
- Persistent installation across SteamOS updates

---

## Problem Statement

### The Gaming Network Problem

Modern WiFi is optimized for bulk throughput and power efficiency - metrics that matter for downloading games but actively harm the gaming experience once you're playing.

**Symptoms users experience:**
1. **Bufferbloat**: Router queues fill with large packets, delaying small gaming packets by 50-500ms
2. **Jitter spikes**: Latency varies wildly (20ms → 200ms → 30ms) causing rubber-banding
3. **Power save interruptions**: WiFi radio sleeps mid-game, causing 100ms+ delays
4. **Reconnection stutters**: After sleep/wake, cached network state is stale
5. **Wrong band selection**: Device stays on congested 2.4GHz when 5GHz is available

### Why Existing Solutions Fail

**Manual QoS configuration:**
- Requires router access (not always possible)
- Static bandwidth limits don't adapt to changing conditions
- Users don't know what values to set

**Gaming mode switches:**
- Binary on/off doesn't handle nuanced scenarios
- Often just disables power save (battery drain)
- Doesn't address bufferbloat

**Network manager tweaks:**
- Scattered across multiple config files
- Don't persist across updates on immutable systems
- No coordination between optimizations

---

## Solution Overview

hifi-wifi takes a holistic approach: a single daemon that coordinates all network optimizations and adapts to changing conditions in real-time.

### Design Principles

1. **Zero configuration**: Works out of the box with optimal defaults
2. **Adaptive**: Adjusts to network conditions, power state, and usage patterns
3. **Non-invasive**: Doesn't modify router settings or require special hardware
4. **Resilient**: Self-heals after system updates, sleep cycles, and network changes
5. **Efficient**: Minimal CPU/memory footprint, battery-conscious

### Technology Stack

| Component | Technology | Purpose |
|-----------|------------|---------|
| Runtime | Rust + Tokio | Async, safe, efficient |
| IPC | D-Bus (zbus) | NetworkManager communication |
| Traffic shaping | CAKE qdisc | Bufferbloat elimination |
| Event detection | inotify | Connection change monitoring |
| Service management | systemd | Lifecycle, logging, restart |
| Configuration | TOML | Human-readable settings |

---

## System Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        User Space                                │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                   hifi-wifi daemon                       │    │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────┐  │    │
│  │  │   Network   │  │    Band     │  │     Power       │  │    │
│  │  │  Governor   │  │  Steering   │  │   Management    │  │    │
│  │  └──────┬──────┘  └──────┬──────┘  └────────┬────────┘  │    │
│  │         │                │                   │           │    │
│  │  ┌──────┴────────────────┴───────────────────┴────────┐ │    │
│  │  │              NetworkManager Client (D-Bus)          │ │    │
│  │  └─────────────────────────┬───────────────────────────┘ │    │
│  └────────────────────────────┼─────────────────────────────┘    │
│                               │                                   │
│  ┌────────────────────────────┼─────────────────────────────┐    │
│  │         NetworkManager     │                              │    │
│  │  ┌─────────────────────────┴───────────────────────────┐ │    │
│  │  │                    D-Bus API                         │ │    │
│  │  └─────────────────────────┬───────────────────────────┘ │    │
│  │                            │                              │    │
│  │  ┌─────────────┐  ┌────────┴────────┐  ┌─────────────┐   │    │
│  │  │  Dispatcher │  │  Device Manager │  │  Connection │   │    │
│  │  │   Scripts   │  │                 │  │   Profiles  │   │    │
│  │  └──────┬──────┘  └─────────────────┘  └─────────────┘   │    │
│  └─────────┼────────────────────────────────────────────────┘    │
│            │                                                      │
└────────────┼──────────────────────────────────────────────────────┘
             │
┌────────────┼──────────────────────────────────────────────────────┐
│            │              Kernel Space                            │
│  ┌─────────┴─────────┐  ┌─────────────────┐  ┌─────────────────┐ │
│  │  /run/hifi-wifi/  │  │   Traffic Control│  │   WiFi Driver   │ │
│  │  connection-changed│  │   (tc / CAKE)    │  │   (iwlwifi,     │ │
│  │  (inotify watch)  │  │                  │  │    ath10k, etc) │ │
│  └───────────────────┘  └─────────────────┘  └─────────────────┘ │
│                                                                   │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │                    Network Stack                             │ │
│  │  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌─────────────────┐ │ │
│  │  │  sch_cake│  │  sysctl │  │ nl80211 │  │  Power Manager  │ │ │
│  │  │  qdisc  │  │ tunables│  │  (iw)   │  │  (TLP/PPD)      │ │ │
│  │  └─────────┘  └─────────┘  └─────────┘  └─────────────────┘ │ │
│  └─────────────────────────────────────────────────────────────┘ │
└───────────────────────────────────────────────────────────────────┘
```

### Process Flow

```
                    ┌──────────────────┐
                    │   System Boot    │
                    └────────┬─────────┘
                             │
                    ┌────────▼─────────┐
                    │ systemd starts   │
                    │ hifi-wifi.service│
                    └────────┬─────────┘
                             │
              ┌──────────────▼──────────────┐
              │  Load configuration         │
              │  /etc/hifi-wifi/config.toml │
              └──────────────┬──────────────┘
                             │
              ┌──────────────▼──────────────┐
              │  Discover network interfaces │
              │  via NetworkManager D-Bus   │
              └──────────────┬──────────────┘
                             │
              ┌──────────────▼──────────────┐
              │  Apply initial optimizations │
              │  - sysctl tuning            │
              │  - driver parameters        │
              │  - CAKE qdisc               │
              └──────────────┬──────────────┘
                             │
              ┌──────────────▼──────────────┐
              │  Start connection watcher   │
              │  (inotify on /run/hifi-wifi)│
              └──────────────┬──────────────┘
                             │
              ┌──────────────▼──────────────┐
              │  Enter governor loop        │◄────────┐
              │  (async event-driven)       │         │
              └──────────────┬──────────────┘         │
                             │                        │
         ┌───────────────────┼───────────────────┐    │
         │                   │                   │    │
         ▼                   ▼                   ▼    │
     ┌─────────┐       ┌─────────────┐     ┌─────────┐│
     │ Timer   │       │ Connection  │     │ Signal  ││
     │ tick    │       │ event       │     │ change  ││
     │ (2s)    │       │ (inotify)   │     │ (D-Bus) ││
     └────┬────┘       └──────┬──────┘     └────┬────┘│
         │                   │                  │     │
         └───────────────────┼──────────────────┘     │
                             │                        │
              ┌──────────────▼──────────────┐         │
              │  Re-evaluate network state  │         │
              │  - Check signal strength    │         │
              │  - Update CAKE bandwidth    │         │
              │  - Consider band steering   │         │
              └──────────────┬──────────────┘         │
                             │                        │
                             └────────────────────────┘
```

---

## Core Components

### Network Governor

The governor is the central decision-making component that coordinates all optimizations.

**Location:** `src/network/governor.rs`

**Responsibilities:**
- Main event loop (async Tokio runtime)
- Periodic health checks (default: 2 second tick rate, configurable via `tick_rate_secs`)
- CAKE bandwidth adjustment ("breathing" algorithm)
- Connection event handling
- Band steering decisions

**Breathing CAKE Algorithm:**

Rather than setting a static bandwidth limit, the governor continuously adjusts CAKE's bandwidth parameter dynamically using a 4-stage pipeline based on the current Wi-Fi PHY rate:

1. **Rate Gathering & Averaging:** Checks the current PHY rate from both NetworkManager D-Bus API and local `iw` tools. If both are valid (>= 20 Mbit), it averages them to prevent outlier readings.
2. **Outlier Filtering (Median):** Pushes the samples to a rolling window (default size 3) and extracts the median to smooth out transient spikes or probe frame drops.
3. **Sensitivity Thresholds:** Caches the last applied bandwidth and only triggers updates if the new target exceeds a minimum difference (default: 15 Mbit) or percentage change (default: 15%).
4. **Asymmetric Hysteresis:** Applies directional timers. Rate decreases are approved immediately (1 tick / 2 seconds) to prevent bufferbloat. Rate increases require stability over multiple consecutive ticks (default: 3 ticks / 6 seconds) to prevent rate oscillations.

```
CAKE_bandwidth = PHY_rate × overhead_factor

where:
  PHY_rate = Calculated median rate (from D-Bus / iw)
  overhead_factor = 0.85 (configurable via cake_overhead_factor)
```

This ensures CAKE always knows the true available bandwidth, preventing both:
- Under-utilization (bandwidth set too low)
- Bufferbloat (bandwidth set higher than actual capacity)

**Connection Event Handling:**

When a connection change is detected:
1. Clear cached PHY rate (stale data causes wrong CAKE bandwidth)
2. Wait 1 second for link stabilization
3. Re-query interface state from NetworkManager
4. Reapply optimizations with fresh data

### CAKE Traffic Shaper

CAKE (Common Applications Kept Enhanced) is a modern qdisc that combines:
- Bandwidth shaping
- Flow isolation (fairness between connections)
- ECN (Explicit Congestion Notification)
- Per-flow queuing with smart hash

**Location:** `src/network/wifi.rs`

**Application:**

```bash
# What hifi-wifi executes:
tc qdisc replace dev wlan0 root cake bandwidth 520mbit

# For egress (upload) traffic shaping
# Ingress is harder - we can't control what the AP sends
```

**Why CAKE over alternatives:**

| Qdisc | Pros | Cons |
|-------|------|------|
| pfifo_fast | Default, simple | No flow isolation, no shaping |
| fq_codel | Good latency | Doesn't handle WiFi rate changes |
| htb | Flexible | Complex, requires manual class setup |
| **CAKE** | All-in-one, WiFi-aware | Slightly higher CPU (negligible) |

**Bandwidth Calculation:**

```rust
// The CAKE breathing logic runs in the governor's async loop
// and calculates scaled bandwidth as:
let bitrate_mbit = effective_bitrate / 1000;
let scaled_mbit = (bitrate_mbit as f64 * self.config.cake_overhead_factor) as u32;
let bandwidth = scaled_mbit.max(10); // Capped to 10 Mbit minimum to prevent connection stalls
```

### Band Steering

Automatically selects the optimal WiFi band when multiple options are available.

**Location:** `src/network/nm.rs` (AccessPoint scoring)

**Scoring Algorithm:**

```
Score = RSSI + band_bias + throughput_bonus

where:
  RSSI             = Signal strength in dBm (e.g., -52)
  band_bias        = +15 for 5GHz, +25 for 6GHz (configurable via band_bias_5ghz / band_bias_6ghz)
  throughput_bonus = min(max_bitrate_kbps / 60000, 10)
```

**Example Calculation:**

| AP | Band | RSSI | Bias | Throughput Bonus | Score |
|----|------|------|------|------------------|-------|
| AP1 | 2.4GHz | -45 | 0 | +0 (54Mbps) | -45 |
| AP2 | 5GHz | -58 | +15 | +10 (866Mbps) | -33 |
| AP3 | 5GHz | -72 | +15 | +10 (866Mbps) | -47 |

Result: AP2 selected (highest score).

**Signal Thresholds:**

Different bands have different minimum usable signal levels:

| Band | Minimum RSSI | Rationale |
|------|--------------|-----------|
| 2.4 GHz | -75 dBm | Long range, tolerates weak signals |
| 5 GHz | -72 dBm | Higher throughput justifies pushing limits |
| 6 GHz | -70 dBm | New band, conservative due to high path loss |

**Why Throughput Bonus Matters:**

Two 5GHz APs might have the same RSSI, but vastly different capabilities:
- WiFi 5 AP: 433 Mbps max → +7 bonus
- WiFi 6 AP: 1200 Mbps max → +10 bonus (capped at 10)

The throughput bonus ensures we prefer the faster AP when signal is comparable.

### Power Management

Balances performance and battery life based on device context.

**Location:** `src/system/power.rs`

**Power States:**

| Context | Power Save | Rationale |
|---------|------------|-----------|
| Desktop (AC) | Disabled | Always max performance |
| Handheld + AC | Disabled | Plugged in = performance |
| Handheld + Battery | Enabled | Preserve battery |
| Handheld + Battery + Gaming | Disabled | Gaming trumps battery |

**Game Detection:**

```rust
// Game mode is determined dynamically based on traffic flow density (PPS):
if pps > pps_threshold {
    // Activates/extends Game Mode and freezes CAKE updates
    state.game_mode_until = Some(Instant::now() + Duration::from_secs(cooldown_secs));
    state.tc_manager.enter_game_mode();
}
```

This network-activity-based game detection avoids complex process tracing or desktop-specific APIs, making it universal across all systemd gaming handhelds (Steam Deck, MSI Claw, ROG Ally, Bazzite, etc.).

**WiFi Power Save Control:**

```bash
# Disable power save (low latency):
iw dev wlan0 set power_save off

# Enable power save (battery):
iw dev wlan0 set power_save on
```

Power save mode allows the WiFi radio to sleep between beacons (typically 100ms intervals). This saves ~100-300mW but adds latency variance.

### Connection Event Handler

Detects and responds to network connection changes.

**The Problem (Issue #10):**

After WiFi reconnects (sleep/wake, network switch), the driver resets its rate control state. hifi-wifi's cached PHY rate becomes stale, causing:
1. CAKE uses wrong bandwidth (e.g., 50Mbit when link is 500Mbit)
2. Severe bufferbloat until next governor tick
3. User experiences stuttering until manual reconnection

**The Solution:**

Hybrid event-driven architecture:

```
┌─────────────────────┐     ┌─────────────────────────┐
│ NetworkManager      │     │ hifi-wifi daemon        │
│                     │     │                         │
│  Connection Up      │     │  inotify watcher        │
│       │             │     │       │                 │
│       ▼             │     │       ▼                 │
│  Run dispatcher.d/* │     │  Detect file change    │
│       │             │     │       │                 │
│       ▼             │     │       ▼                 │
│  99-hifi-wifi-connect│     │  Clear bitrate cache   │
│       │             │     │       │                 │
│       ▼             │     │       ▼                 │
│  touch /run/hifi-   │────▶│  Wait 1s (stabilize)   │
│  wifi/connection-   │     │       │                 │
│  changed            │     │       ▼                 │
│                     │     │  Re-optimize with      │
│                     │     │  fresh PHY rate        │
└─────────────────────┘     └─────────────────────────┘
```

**Dispatcher Script:**

```bash
#!/bin/bash
# /etc/NetworkManager/dispatcher.d/99-hifi-wifi-connect
if [[ "$2" == "up" ]] && [[ "$DEVICE_TYPE" == "wifi" ]]; then
    mkdir -p /run/hifi-wifi
    touch /run/hifi-wifi/connection-changed
fi
```

**Why inotify instead of D-Bus signals?**

- D-Bus StateChanged fires multiple times during connection
- Difficult to know when connection is "stable"
- inotify is simple: one file touch = one event
- Decouples dispatcher timing from daemon processing

### IRQ Affinity Optimization

During high-throughput wireless gaming or high-bitrate video streaming (e.g., Moonlight/Sunshine), network latency is highly sensitive to CPU thread scheduling and interrupt handling overhead. By default, Linux handles Wi-Fi card interrupts dynamically across all available CPU cores, causing context switching overhead and cache-line bouncing.

`hifi-wifi` addresses this by locking (pinning) Wi-Fi card interrupts to a dedicated CPU core (typically CPU 1) to maximize cache locality and minimize scheduling latency.

**Location:** `src/system/optimizer.rs`

**Algorithm and Mechanism:**
1. **IRQ Discovery:** The optimizer reads `/proc/interrupts` to find all interrupt vectors associated with the active Wi-Fi interface. It scans lines case-insensitively for the interface name (e.g., `wlan0`), driver name (e.g., `rtw89_pci`), or associated bus/subsystem descriptors (such as Qualcomm's `mhi` vectors).
2. **CPU Pinning Execution:** For each found interrupt vector, the optimizer writes the target CPU mask (e.g., `02` for CPU 1) to `/proc/irq/<num>/smp_affinity`.
3. **Handling Managed Interrupts:** Certain modern drivers (such as `ath11k` on Qualcomm chips) use managed interrupts where the kernel reserves scheduling rights. Writes to `smp_affinity` for these vectors return an Input/Output Error (`EIO`). The optimizer detects OS Error 5 (`EIO`), treats it as managed-by-kernel (success state), and logs it gracefully at debug level, avoiding write failure warnings.
4. **Validation:** The status CLI queries `/proc/interrupts` using the same shared lookup logic and marks the interface as `[OPTIMIZED] (CPU 1, X/Y vectors)` to verify successful user-space pinning of all non-managed interrupt vectors.

---

---

## Platform Integration

### SteamOS Persistence

SteamOS uses an immutable root filesystem that gets replaced on every system update. This creates challenges for persistent software installation.

**What Gets Wiped:**
- `/usr/*` - All system binaries
- `/etc/systemd/system/*` - System services
- `/etc/sysctl.d/*` - Kernel parameters
- `/etc/modprobe.d/*` - Driver configurations

**What Persists:**
- `/home/*` - User data
- `/var/*` - Variable data
- `~/.config/*` - User configurations

**hifi-wifi Persistence Strategy:**

```
┌─────────────────────────────────────────────────────────────┐
│                    Persistent Locations                      │
│                                                              │
│  /var/lib/hifi-wifi/                                        │
│  └── hifi-wifi          (binary - survives updates)         │
│                                                              │
│  ~/.bashrc                                                   │
│  └── PATH=/var/lib/hifi-wifi:$PATH  (CLI access)           │
│                                                              │
│  ~/.config/systemd/user/                                    │
│  └── hifi-wifi-repair.service  (auto-repair on login)      │
│                                                              │
│  /home/linuxbrew/.linuxbrew/                                │
│  └── (Homebrew + Rust toolchain for rebuilds)               │
│                                                              │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│                    Ephemeral Locations                       │
│                    (Recreated by repair service)             │
│                                                              │
│  /etc/systemd/system/hifi-wifi.service                      │
│  /etc/sysctl.d/99-hifi-wifi.conf                            │
│  /etc/modprobe.d/wifi_*.conf                                │
│  /etc/NetworkManager/dispatcher.d/99-hifi-wifi-connect      │
│                                                              │
└─────────────────────────────────────────────────────────────┘
```

**Boot Repair Flow:**

```
System Boot
    │
    ▼
User Login (Desktop or Game Mode)
    │
    ▼
user@.service starts (lingering enabled)
    │
    ▼
hifi-wifi-repair.service runs
    │
    ▼
Check: Does /etc/systemd/system/hifi-wifi.service exist?
    │
    ├─── Yes ──▶ Do nothing (already configured)
    │
    └─── No ───▶ Run: pkexec hifi-wifi install
                     │
                     ▼
                  Recreate all ephemeral files
                  Start hifi-wifi.service
```

**Homebrew Dependency Isolation:**
To maintain a fast and painless installer flow, the installer splits dependencies into build-time (GCC, headers) and runtime (`iproute2` for `tc`).
- When compiling from source, a full build environment is established.
- When installing from a pre-compiled release binary, the installer checks if `tc` is already present in the system path or inside `/home/linuxbrew/.linuxbrew/sbin`. If missing, it installs only `iproute2` via Homebrew, avoiding the massive download overhead of GCC while guaranteeing that CAKE traffic shaping remains functional.

### NetworkManager Integration

hifi-wifi communicates with NetworkManager via D-Bus to:
- Discover network interfaces
- Query connection state
- Get WiFi signal strength and PHY rates
- Scan for available access points

**D-Bus Interface Usage:**

```rust
// Connection to system bus
let connection = Connection::system().await?;

// NetworkManager proxy
let nm = NetworkManagerProxy::new(&connection).await?;

// Get all devices
let devices = nm.get_all_devices().await?;

// For each WiFi device, get wireless-specific info
let wireless = WirelessProxy::new(&connection, device_path).await?;
let bitrate = wireless.bitrate().await?;  // Current PHY rate in Kbit/s
let aps = wireless.get_all_access_points().await?;
```

**Why NetworkManager (not direct nl80211)?**

- NetworkManager already handles connection management
- D-Bus API is stable and well-documented
- Avoids duplicating connection state tracking
- Works across different WiFi backends (wpa_supplicant, iwd)

### Systemd Service

**Service File:**

```ini
[Unit]
Description=hifi-wifi Network Optimizer
After=network-online.target NetworkManager.service
Wants=network-online.target

[Service]
Type=simple
ExecStart=/var/lib/hifi-wifi/hifi-wifi daemon
Restart=on-failure
RestartSec=5

# Security hardening
NoNewPrivileges=no  # Needs capabilities for tc, iw
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=/run/hifi-wifi /etc/sysctl.d /etc/modprobe.d

[Install]
WantedBy=multi-user.target
```

**Why Type=simple?**

The daemon runs continuously and logs to stdout/stderr. Systemd captures this to the journal automatically.

**Restart Behavior:**

- `on-failure`: Restart only on non-zero exit
- `RestartSec=5`: Wait 5 seconds before restart (prevents tight loops)

---

## Data Flow

### Startup Sequence

```
1. systemd starts hifi-wifi.service
2. Daemon loads /etc/hifi-wifi/config.toml (or uses defaults)
3. Connect to D-Bus system bus
4. Query NetworkManager for all network devices
5. For each WiFi/Ethernet interface:
   a. Check if connected
   b. Get current PHY rate
   c. Apply sysctl optimizations
   d. Apply driver-specific modprobe config
   e. Apply CAKE qdisc with calculated bandwidth
   f. Set power save mode based on context
6. Install inotify watcher on /run/hifi-wifi/connection-changed
7. Enter main event loop
```

### Optimization Application

```
Interface: wlan0, Driver: iwlwifi, Connected: Yes
    │
    ▼
Apply sysctl tunables:
    net.core.rmem_max = 16777216
    net.core.wmem_max = 16777216
    net.ipv4.tcp_rmem = 4096 87380 16777216
    net.ipv4.tcp_wmem = 4096 65536 16777216
    net.ipv4.tcp_congestion_control = bbr
    │
    ▼
Apply driver config (/etc/modprobe.d/wifi_iwlwifi.conf):
    options iwlwifi power_save=0 uapsd_disable=1
    │
    ▼
Query current PHY rate: 866 Mbit/s
    │
    ▼
Calculate CAKE bandwidth: 866 × 0.85 = 736 Mbit/s
    │
    ▼
Apply CAKE qdisc:
    tc qdisc replace dev wlan0 root cake bandwidth 736mbit
    │
    ▼
Check power context: Handheld, Battery, Not Gaming
    │
    ▼
Set power save: On (preserve battery)
    │
    ▼
Interface optimized ✓
```

### Governor Loop (Steady State)

```
Every 2 seconds (tick_rate_secs):
    │
    ├──▶ For each connected interface:
    │        │
    │        ├──▶ Get current PHY rate
    │        │
    │        ├──▶ Compare to cached rate
    │        │        │
    │        │        ├── Same ──▶ No action
    │        │        │
    │        │        └── Different ──▶ Update CAKE bandwidth
    │        │
    │        └──▶ Check power context
    │                 │
    │                 └──▶ Adjust power save if needed
    │
    └──▶ Sleep 2 seconds
```

### Connection Event Handling

```
NetworkManager: Connection established on wlan0
    │
    ▼
Dispatcher script runs:
    touch /run/hifi-wifi/connection-changed
    │
    ▼
inotify detects file modification
    │
    ▼
hifi-wifi daemon wakes:
    │
    ├──▶ Clear cached PHY rates (all interfaces)
    │
    ├──▶ Sleep 1 second (let link stabilize)
    │
    ├──▶ Re-query NetworkManager for interface state
    │
    └──▶ Reapply all optimizations with fresh data
```

---

## Configuration

### Configuration File

**Location:** `/etc/hifi-wifi/config.toml`

**Default Values:**

```toml
[global]
# Governor loop interval in seconds
tick_rate_secs = 2

[power]
enabled = true
# wlan_power_save options: "adaptive", "off", "on"
wlan_power_save = "adaptive"

[wifi]
enabled = true
# Signal thresholds for band steering (dBm)
min_signal_2g_dbm = -75
min_signal_5g_dbm = -72
min_signal_6g_dbm = -70
# Band bias (higher = more preference)
band_bias_5ghz = 15
band_bias_6ghz = 25
# wifi_mac_address = "permanent"

[system]
sysctl_enabled = true
irq_affinity_enabled = true
driver_tweaks_enabled = true

[backend]
iwd_periodic_scan_disable = true

[governor]
breathing_cake_enabled = true
cake_median_window = 3
cake_change_threshold_mbit = 15
cake_change_threshold_pct = 0.15
cake_overhead_factor = 0.85
cake_hysteresis_up = 3    # Slow increases (prevent oscillation)
cake_hysteresis_down = 1  # Fast decreases (prevent bufferbloat)
game_mode_enabled = true
game_mode_pps_threshold = 200
game_mode_cooldown_secs = 30
game_mode_freeze_cake = true
band_steering_enabled = true
roam_hysteresis_ticks = 3
cpu_coalescing_enabled = true
cpu_coalescing_threshold = 0.90
cpu_avg_window_size = 3
scan_suppress = "adaptive"
```

### Runtime Configuration

Most settings can be changed without restart by editing the config file. The daemon watches for changes and reloads automatically.

**Exception:** Driver modprobe settings require reboot to take effect.

---

## Security Model

### Privilege Requirements

hifi-wifi requires root privileges for:
- `tc qdisc` commands (traffic control)
- `iw` commands (power save control)
- Writing to `/etc/sysctl.d/`, `/etc/modprobe.d/`
- Creating NetworkManager dispatcher scripts

### Systemd Hardening

Despite needing root, the service is hardened:

```ini
ProtectSystem=strict      # Read-only access to /usr, /boot, /efi
ProtectHome=read-only     # Read-only access to /home
ReadWritePaths=...        # Whitelist specific write paths
```

### Polkit Integration

For desktop integration (planned Decky plugin), a Polkit policy allows the `deck` user to run specific hifi-wifi commands without password:

```xml
<action id="com.doughty247.hifi-wifi.control">
  <defaults>
    <allow_active>yes</allow_active>
  </defaults>
  <annotate key="org.freedesktop.policykit.exec.path">
    /var/lib/hifi-wifi/hifi-wifi
  </annotate>
</action>
```

---

## Performance Characteristics

### Resource Usage

| Metric | Value | Notes |
|--------|-------|-------|
| Memory | ~8 MB | Rust binary + runtime |
| CPU (idle) | <0.1% | Sleeping between checks |
| CPU (active) | <1% | During optimization |
| Disk | 8 MB | Binary size |
| Network | 0 | No telemetry or external calls |

### Latency Impact

**Before hifi-wifi (typical home network with bufferbloat):**
```
PING router (192.168.1.1):
  min/avg/max/stddev = 2.1/45.3/287.4/52.1 ms
```

**After hifi-wifi:**
```
PING router (192.168.1.1):
  min/avg/max/stddev = 1.8/3.2/12.4/2.1 ms
```

**Key improvements:**
- Average latency: 45ms → 3ms (93% reduction)
- Jitter (stddev): 52ms → 2ms (96% reduction)
- Max latency: 287ms → 12ms (96% reduction)

### Throughput Impact

CAKE does not significantly reduce throughput when properly configured:

| Scenario | Without hifi-wifi | With hifi-wifi |
|----------|-------------------|----------------|
| Download (idle) | 940 Mbps | 920 Mbps |
| Download (gaming) | 940 Mbps | 900 Mbps |
| Upload (idle) | 94 Mbps | 92 Mbps |

The ~2-5% throughput reduction is the cost of flow isolation and queue management. For gaming, this tradeoff is extremely favorable.

---

## Future Roadmap

### v3.1.0 - Core Engine & "Lambda Core" (Implemented)

- **Zero-Overhead Netlink Listener**: Subscribes directly to kernel netlink link multicast groups (`RTMGRP_LINK`) to trigger instant governor ticks on carrier changes.
- **RTT-Driven CAKE Scaling**: Queries the kernel's `TCP_INFO` struct dynamically to scale CAKE queue capacity during wireless jitter spikes.
- **cgroup v2 & DSCP Tagging**: Prioritizes gaming/streaming application traffic (e.g. `gamescope.slice`) and ports via `nftables` postrouting rules.
- **Proactive Roaming Governor**: Triggers active NetworkManager association handovers to target BSSIDs when signal strength drops below `-80 dBm`.
- **Zero-Copy eBPF Game Bypass**: Intercepts UDP streams (Moonlight, Steam Link) at the kernel driver entry point for sub-millisecond bypass latency.
- **Predictive Multi-Path Bonding**: Duplicates game traffic across secondary links when packet loss or jitter exceeds threshold.
- **Media-Aware Congestion Switching**: Switches system default congestion control on the fly (BBR baseline, pivoting to Cubic under active CAKE saturation).

### v3.2.0 - Decky Plugin & QAM Integration

- Decky Loader UI plugin for Steam Deck
- Real-time latency and jitter display in QAM (Quick Access Menu)
- One-click toggling and configuration overrides

### v3.3.0 - Advanced Network Routing

- VPN-aware optimization
- Mesh network channel optimization

### v4.0.0 - Platform Expansion

- Router-side agent (OpenWrt package)
- Cloud configuration sync

---

## Appendix: Driver-Specific Configurations

To achieve low-latency performance and hardware stability under load, `hifi-wifi` configures kernel modules in `/etc/modprobe.d/` based on the detected network driver category:

### Intel (iwlwifi)
Written to `iwlwifi.conf`:
```ini
options iwlwifi power_save=0 uapsd_disable=1
options iwlmvm power_scheme=1
```
- `power_save=0`: Disables driver-level power saving to prevent latency drops.
- `uapsd_disable=1`: Disables U-APSD (unscheduled automatic power save delivery) which can cause latency spikes.
- `power_scheme=1`: Forces the MVM framework into "Always Active" mode, preventing the network card from dropping links on battery or after system suspend.

### Qualcomm Atheros (ath9k/ath11k)
Written to `ath_wifi.conf`:
```ini
options ath11k_pci disable_aspm=1
options ath9k ps_enable=0
```
- `disable_aspm=1`: Disables PCIe Active State Power Management for modern Qualcomm devices (e.g., Steam Deck OLED).
- `ps_enable=0`: Disables hardware power saving for legacy Atheros 802.11n chipsets.

### Realtek (rtw88/rtw89)
Modern Realtek cards write to `rtw89.conf`:
```ini
options rtw89_pci disable_aspm_l1=y disable_aspm_l1ss=y
options rtw89_core disable_ps_mode=y
```
- `disable_aspm_l1` / `disable_aspm_l1ss`: Disables PCIe L1 and L1 sub-states to prevent lag spikes on buggy BIOS platforms.
- `disable_ps_mode=y`: Disables firmware-level power save.

Legacy Realtek cards write to `rtw88.conf`:
```ini
options rtw88_pci disable_aspm=1
options rtw88_core disable_lps_deep=Y
```
- `disable_aspm=1`: Disables PCIe ASPM states.
- `disable_lps_deep=Y`: Disables deep low-power states to prevent reconnection drops.

Other legacy Realtek cards (e.g., RTL8192EE) write to `rtl_legacy.conf`:
```ini
options rtl8192ee swenc=1 ips=0 fwlps=0
options rtl8188ee swenc=1 ips=0 fwlps=0
options rtl_pci disable_aspm=1
```

### MediaTek (mt76/mt7921)
Written to `mediatek.conf`:
```ini
options mt7921e disable_aspm=1
options mt76_usb disable_usb_sg=1
```
- `disable_aspm=1`: Prevents latency spikes on PCI-e MediaTek chipsets (like the MT7922).
- `disable_usb_sg=1`: Disables USB scatter-gather to improve stability on USB-based MediaTek adapters.

### Broadcom (brcmfmac/wl)
Written to `broadcom.conf`:
```ini
options brcmfmac roamoff=1
options wl interference=0
```
- `roamoff=1`: Disables background roaming searches at the driver level to prevent scanning hiccups.
- `interference=0`: Turns off legacy interference mitigation models that limit bandwidth.

---

## Appendix: Troubleshooting

### CAKE Not Applying

**Symptom:** `tc qdisc show` doesn't show CAKE on interface

**Possible causes:**
1. Interface not connected
2. `sch_cake` kernel module not loaded
3. Another qdisc already set (some distros set fq_codel)

**Solution:**
```bash
# Check if CAKE module is available
modprobe sch_cake

# Force remove existing qdisc
tc qdisc del dev wlan0 root 2>/dev/null

# Let hifi-wifi reapply
sudo hifi-wifi apply
```

### Service Keeps Restarting

**Symptom:** `journalctl -u hifi-wifi` shows repeated start/stop

**Possible causes:**
1. D-Bus connection failure (NetworkManager not running)
2. Configuration file syntax error
3. Missing permissions

**Solution:**
```bash
# Check NetworkManager status
systemctl status NetworkManager

# Validate config file
cat /etc/hifi-wifi/config.toml

# Check journal for specific error
journalctl -u hifi-wifi -n 50
```

### Poor Performance After Sleep

**Symptom:** Lag after waking from sleep until manually reconnecting

**Possible causes:**
1. NetworkManager dispatcher not installed
2. inotify watcher failed
3. Connection event file not being touched

**Solution:**
```bash
# Check dispatcher exists
ls -la /etc/NetworkManager/dispatcher.d/99-hifi-wifi-connect

# Test manually
sudo touch /run/hifi-wifi/connection-changed

# Check daemon received event
journalctl -u hifi-wifi -f
# Should see "Connection event detected, re-optimizing..."
```

---

## References

1. **CAKE qdisc**: https://www.bufferbloat.net/projects/codel/wiki/Cake/
2. **Bufferbloat**: https://www.bufferbloat.net/
3. **Linux WiFi Subsystem**: https://wireless.wiki.kernel.org/
4. **NetworkManager D-Bus API**: https://networkmanager.dev/docs/api/latest/
5. **SteamOS**: https://store.steampowered.com/steamos

---

*Document Version: 1.0*  
*Last Updated: January 2026*  
*Maintainer: doughty247*
