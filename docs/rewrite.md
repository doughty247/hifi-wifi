Technical Design Document: hifi-wifi v3.0 (Production Engine)
Target Audience: AI Engineering Specialists (Gemini 3 Pro / Claude Opus)
Source Context: Fork of hifi-wifi (v1.3.0-rc2 legacy shell scripts).
Goal: Rewrite as a production-grade, fault-tolerant system daemon in Rust.
1. Executive Summary
The goal is to replace the legacy Bash-based logic with a compiled Rust binary utilizing Async I/O. The new daemon must not "script around" the OS; it must integrate with the OS using native APIs (DBus/Netlink/Syscalls).
Core Mandates:
No Text Parsing: Do not parse ip, iw, or ifconfig text output unless absolutely necessary. Use structured data APIs.
No OS Fighting: Do not force iw roam blindly. Use NetworkManager DBus calls to request roams.
Stateful Logic: The daemon must maintain state (hysteresis, smoothing) to prevent network jitter.
2. Technology Stack & Dependencies
Language: Rust (2021 Edition)
Runtime: Tokio (Async)
Required Crates (Cargo.toml):
tokio = { version = "1.32", features = ["full"] } -> Core Event Loop.
zbus = "3.14" -> Pure Rust DBus implementation (Communicating with NetworkManager).
procfs = "0.16" -> Native reading of /proc for CPU stats (or manual /proc/stat parsing).
anyhow / log / env_logger -> Error handling and observability.
serde -> Serialization.
3. Architecture Modules
The codebase should be structured into three distinct domains:
A. System Domain (system.rs)
Responsible for reading hardware state without blocking and applying low-level system optimizations.
CPU Monitor: Must implement a Rolling Average (window size ~3 samples) to smooth out CPU spikes.
Source: Read /proc/stat.
Output: Float 0.0 to 1.0 representing total system load.
Power Source: Detects AC vs Battery.
Source: Iterate /sys/class/power_supply/*. Check type (Mains/Battery) and status (Charging/Discharging).
Persistence: A "Self-Healing" routine that checks if /usr/bin/hifi-wifi exists on boot. If missing (due to OS update), recreate the symlink from /var/lib/hifi-wifi/.
Kernel & Driver Optimizer:
Sysctl: Apply critical network stack tuning (BBR, fq_codel, buffer sizes).
Drivers: Detect active driver (Realtek, Intel, MediaTek) and write optimized config to /etc/modprobe.d/ (e.g., disable_aspm, power_save=0).
IRQ Affinity: Pin Wi-Fi IRQs to CPU 1 to prevent context switching overhead logic found in legacy scripts.
B. Network Domain (network/nm.rs, tc.rs, iwd.rs)
Responsible for talking to the kernel, NetworkManager, and backend daemons.
DBus Client (nm.rs):
Interface: org.freedesktop.NetworkManager.
Action 1: Poll GetDevices to find active interfaces (Handles Hotplug).
Action 2: Query Device.Wireless properties (Bitrate, ActiveAccessPoint).
Action 3: Scan GetAccessPoints to find roam candidates.
Action 4: Enforce connection settings (Disable IPv6, Force Permanent MAC to prevent random disconnections).
Traffic Control (tc.rs):
Wrapper around tc binary (Netlink-TC is too unstable for this specific use case).
Command: tc qdisc replace dev <iface> root cake bandwidth <X>mbit besteffort nat.
Ethtool: Disable hardware offloading (TSO/GSO) but enable GRO to prevent latency spikes.
Backend Tuner (iwd.rs):
If iwd is detected, write /etc/iwd/main.conf to disable internal periodic scanning (DisablePeriodicScan=true) so it doesn't fight the Governor.
C. The Governor (network/mod.rs)
The "Brain" that runs the Async Loop (Tick Rate: 2 seconds).
4. Logic & Algorithms (The "Secret Sauce")
The AI Team must implement these exact logic flows to meet v3.0 specs.
Feature 1: "Breathing" CAKE (Dynamic QoS)
Input: Current negotiated Link Speed (from NetworkManager DBus).
Processing: Apply Exponential Smoothing (EMA).
smoothed_bw = (current_speed * 0.3) + (previous_bw * 0.7)
Trigger: Only update tc-cake if the smoothed value shifts by > 5 Mbit or 10%.
Goal: Prevents the queue discipline from resetting constantly due to minor signal fluctuation.
Feature 2: CPU Governor (Smart Coalescing)
Input: Smoothed CPU Load.
Logic:
IF GameMode == Active AND CpuLoad > 90%: Enable Coalescing (ethtool -C adaptive-rx on).
ELSE IF GameMode == Active: Disable Coalescing (ethtool -C adaptive-rx off).
ELSE (Idle/Battery): Enable Coalescing.
Why: Prevents "death by interrupts" in CPU-bound games (e.g., Cyberpunk 2077 on Steam Deck).
Feature 3: Smart Band Steering (with Hysteresis)
Problem: Clients stick to 2.4GHz because RSSI is higher (-50dBm vs -60dBm on 5GHz).
Scoring Logic:
Score = RSSI + BandBias
Bias: 2.4GHz = 0, 5GHz = +15, 6GHz = +20.
Hysteresis (The Stabilizer):
Do NOT roam immediately if a better score is found.
Require the better candidate to persist for 3 consecutive ticks (6 seconds).
Only then trigger org.freedesktop.NetworkManager.Device.Wireless.RequestScan() or Connection Activation.
Feature 4: "Game Mode" Detection (PPS)
Input: Read /sys/class/net/<iface>/statistics/rx_packets + tx_packets.
Calculation: (Current - Last) / TimeDelta.
Threshold: > 200 PPS.
Action: If > 200 PPS, Force Performance Mode (Ignore Battery Saver) for 30 seconds (Cooldown).
Feature 5: Enterprise Safety
Virtual Filter: Explicitly ignore interfaces starting with docker, veth, virbr, tun, tap.
Root Check: Panic/Exit if euid != 0.
5. Deployment & Persistence
Installation Path:
Binary: /var/lib/hifi-wifi/hifi-wifi (Crucial: /var survives SteamOS A/B updates; /usr does not).
Service: /etc/systemd/system/hifi-wifi.service.
Service Config:
[Service]
ExecStart=/var/lib/hifi-wifi/hifi-wifi --monitor
Restart=on-failure
ProtectSystem=full
CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN


6. Prompt for the AI Specialist
Copy and paste this prompt to the AI agent:
"Act as a Senior Systems Engineer specializing in Rust and Linux Networking.
Using the 'hifi-wifi v3.0 Technical Design Document' provided above, generate the complete source code for the project.
Initialize a Cargo project with the specified dependencies (Tokio, ZBus, etc.).
Implement the CpuMonitor with rolling average smoothing.
Implement the NmClient using zbus macros to talk to NetworkManager.
Implement the NetworkGovernor struct containing the logic for Hysteresis, PPS calculation, and the decision matrix for Coalescing/PowerSave.
Ensure the main loop is non-blocking.
Output the code in separate file blocks for main.rs, system.rs, network/mod.rs, network/nm.rs, and network/tc.rs."
