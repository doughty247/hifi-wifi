#!/bin/bash
# wifi-grader.sh
# A scriptable tool to measure Wi-Fi quality: Speed, Latency, and Bufferbloat
# Replicates the logic of web-based tools like devina.io/speed-test
#
# Usage: ./wifi-grader.sh [target_host]
# Default targets: 8.8.8.8 and 1.1.1.1 (Google DNS and Cloudflare)

# Default targets: Google and Cloudflare for average
if [ "$#" -eq 0 ]; then
    TARGETS=("8.8.8.8" "1.1.1.1")
else
    TARGETS=("$@")
fi
LOG_FILE="bufferbloat_results_$(date +%Y%m%d_%H%M%S).log"

# Ensure dependencies are present
# Check for homebrew speedtest-cli and install if missing
if ! command -v brew &> /dev/null; then
    echo "Error: Homebrew is not installed. Please install it first:"
    echo "  /bin/bash -c \"\$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\""
    exit 1
fi

# Detect if running as sudo and get real user
if [[ -n "$SUDO_USER" ]]; then
    REAL_USER="$SUDO_USER"
    BREW_CMD="sudo -u $SUDO_USER brew"
else
    REAL_USER="$USER"
    BREW_CMD="brew"
fi

# Check for speedtest-cli from homebrew
SPEEDTEST_CMD=""

# Get homebrew prefix (works with or without sudo)
if [[ -n "$SUDO_USER" ]]; then
    BREW_PREFIX=$(sudo -u "$SUDO_USER" brew --prefix 2>/dev/null)
else
    BREW_PREFIX=$(brew --prefix 2>/dev/null)
fi

# Add homebrew bin to PATH if not already there
if [[ -n "$BREW_PREFIX" && -d "$BREW_PREFIX/bin" ]]; then
    export PATH="$BREW_PREFIX/bin:$PATH"
fi

# Check if speedtest-cli is now available
if command -v speedtest-cli &> /dev/null; then
    SPEEDTEST_CMD="speedtest-cli"
    echo "Found speedtest-cli at: $(which speedtest-cli)"
else
    echo "Homebrew speedtest-cli not found. Installing as user $REAL_USER..."
    $BREW_CMD install speedtest-cli
    
    # Refresh PATH again
    if [[ -n "$BREW_PREFIX" && -d "$BREW_PREFIX/bin" ]]; then
        export PATH="$BREW_PREFIX/bin:$PATH"
    fi
    
    if command -v speedtest-cli &> /dev/null; then
        SPEEDTEST_CMD="speedtest-cli"
        echo "Installed speedtest-cli at: $(which speedtest-cli)"
    else
        echo "Error: Failed to install or find speedtest-cli from homebrew."
        echo "Try running: brew install speedtest-cli"
        exit 1
    fi
fi

if ! command -v ping &> /dev/null; then
    echo "Error: ping is required."
    exit 1
fi

echo "=========================================="
echo "   WI-FI NETWORK GRADER"
echo "=========================================="
echo "Targets for Latency: ${TARGETS[*]}"
echo "Log File: $LOG_FILE"

# Detect Hifi-Wifi Status
if [[ -f "/var/lib/wifi_patch/applied.flag" ]]; then
    HIFI_STATUS="ACTIVE (Optimized)"
else
    HIFI_STATUS="INACTIVE (Stock)"
fi
echo "Hifi-Wifi Status: $HIFI_STATUS"
echo "------------------------------------------"

log() {
    echo "$1" | tee -a "$LOG_FILE"
}

# Log header to file
{
    echo "Wi-Fi Network Grade Report"
    echo "Date: $(date)"
    echo "Hifi-Wifi Status: $HIFI_STATUS"
    echo "Targets: ${TARGETS[*]}"
    echo "------------------------------------------"
} >> "$LOG_FILE"

start_ping_monitor() {
    local label="$1"
    PING_PIDS=()
    
    log "[$label] Starting latency monitor to: ${TARGETS[*]}"
    
    for target in "${TARGETS[@]}"; do
        local safe_target=$(echo "$target" | tr . _)
        # Run ping in background, redirecting output
        # -i 0.2: Fast interval (5 packets/sec) for high resolution
        ping -i 0.2 "$target" > "/tmp/ping_${label}_${safe_target}.txt" 2>&1 &
        PING_PIDS+=($!)
    done
}

stop_ping_monitor() {
    # Send SIGINT (Ctrl+C) to all pings to force them to print summary
    for pid in "${PING_PIDS[@]}"; do
        kill -2 $pid 2>/dev/null
        wait $pid 2>/dev/null
    done
}

calc_stats() {
    local label="$1"
    local total_avg=0
    local total_jitter=0
    local count=0
    
    for target in "${TARGETS[@]}"; do
        local safe_target=$(echo "$target" | tr . _)
        local file="/tmp/ping_${label}_${safe_target}.txt"
        
        if [[ -f "$file" ]]; then
            # Parse ping summary: rtt min/avg/max/mdev = 15.1/16.2/18.3/1.1 ms
            local stats=$(grep "rtt" "$file" | tail -1)
            local values=$(echo "$stats" | awk -F'=' '{print $2}' | awk '{print $1}')
            
            # Extract avg and mdev (jitter)
            local avg=$(echo "$values" | cut -d'/' -f2)
            local jitter=$(echo "$values" | cut -d'/' -f4)
            
            if [[ "$avg" =~ ^[0-9.]+$ ]]; then
                total_avg=$(echo "$total_avg + $avg" | bc)
                total_jitter=$(echo "$total_jitter + $jitter" | bc)
                count=$((count + 1))
            fi
        fi
    done
    
    if [[ $count -gt 0 ]]; then
        FINAL_AVG=$(echo "scale=1; $total_avg / $count" | bc)
        FINAL_JITTER=$(echo "scale=1; $total_jitter / $count" | bc)
    else
        FINAL_AVG="0"
        FINAL_JITTER="0"
    fi
}

# 1. IDLE LATENCY
log "Phase 1: Measuring IDLE latency (5s)..."
start_ping_monitor "idle"
sleep 5
stop_ping_monitor
calc_stats "idle"
IDLE_AVG=$FINAL_AVG
IDLE_JITTER=$FINAL_JITTER
log "-> Idle Latency: ${IDLE_AVG}ms (Jitter: ${IDLE_JITTER}ms)"
echo ""

# 2. DOWNLOAD LOAD
log "Phase 2: Measuring DOWNLOAD speed and latency..."
start_ping_monitor "download"

# Start download test and capture output (blocking)
# --timeout 60: Allow up to 60s for gigabit connections (default 10s is too short)
# Pre-allocation is enabled by default for accurate testing
log "-> Running Speedtest (Download - this may take 30-60 seconds)..."
$SPEEDTEST_CMD --no-upload --timeout 60 --json > /tmp/speedtest_dl.json 2>&1
SPEEDTEST_EXIT=$?

# Stop monitor immediately after speedtest finishes
stop_ping_monitor

# Parse Speed from JSON (speedtest-cli returns bits/sec, convert to Mbit/s)
if [[ $SPEEDTEST_EXIT -eq 0 && -f /tmp/speedtest_dl.json ]]; then
    DL_BITS=$(grep -o '"download": [0-9.]*' /tmp/speedtest_dl.json | awk '{print $2}')
    if [[ -n "$DL_BITS" && "$DL_BITS" != "0" ]]; then
        DL_MBITS=$(echo "scale=2; $DL_BITS / 1000000" | bc)
        DL_SPEED="${DL_MBITS} Mbit/s"
    else
        log "ERROR: Could not parse download speed from JSON"
        log "JSON contents: $(cat /tmp/speedtest_dl.json)"
        DL_SPEED="N/A (Parse Error)"
    fi
else
    log "ERROR: Speedtest failed with exit code $SPEEDTEST_EXIT"
    [[ -f /tmp/speedtest_dl.json ]] && log "Output: $(cat /tmp/speedtest_dl.json)"
    DL_SPEED="N/A (Test Failed)"
fi

calc_stats "download"
DL_AVG=$FINAL_AVG
DL_JITTER=$FINAL_JITTER
DL_BLOAT=$(echo "$DL_AVG - $IDLE_AVG" | bc)

log "-> Download Speed: ${DL_SPEED}"
log "-> Download Loaded Latency: ${DL_AVG}ms"
log "-> Download Bufferbloat: +${DL_BLOAT}ms"
echo ""

# 3. UPLOAD LOAD
log "Phase 3: Measuring UPLOAD speed and latency..."
start_ping_monitor "upload"

# Start upload test and capture output (blocking)
# --timeout 60: Allow up to 60s for gigabit connections (default 10s is too short)
# Pre-allocation is enabled by default for accurate testing
log "-> Running Speedtest (Upload - this may take 30-60 seconds)..."
$SPEEDTEST_CMD --no-download --timeout 60 --json > /tmp/speedtest_ul.json 2>&1
SPEEDTEST_EXIT=$?

# Stop monitor
stop_ping_monitor

# Parse Speed from JSON (speedtest-cli returns bits/sec, convert to Mbit/s)
if [[ $SPEEDTEST_EXIT -eq 0 && -f /tmp/speedtest_ul.json ]]; then
    UL_BITS=$(grep -o '"upload": [0-9.]*' /tmp/speedtest_ul.json | awk '{print $2}')
    if [[ -n "$UL_BITS" && "$UL_BITS" != "0" ]]; then
        UL_MBITS=$(echo "scale=2; $UL_BITS / 1000000" | bc)
        UL_SPEED="${UL_MBITS} Mbit/s"
    else
        log "ERROR: Could not parse upload speed from JSON"
        log "JSON contents: $(cat /tmp/speedtest_ul.json)"
        UL_SPEED="N/A (Parse Error)"
    fi
else
    log "ERROR: Speedtest failed with exit code $SPEEDTEST_EXIT"
    [[ -f /tmp/speedtest_ul.json ]] && log "Output: $(cat /tmp/speedtest_ul.json)"
    UL_SPEED="N/A (Test Failed)"
fi

calc_stats "upload"
UL_AVG=$FINAL_AVG
UL_JITTER=$FINAL_JITTER
UL_BLOAT=$(echo "$UL_AVG - $IDLE_AVG" | bc)

log "-> Upload Speed: ${UL_SPEED}"
log "-> Upload Loaded Latency: ${UL_AVG}ms"
log "-> Upload Bufferbloat: +${UL_BLOAT}ms"
echo ""

# SUMMARY
echo "==========================================" | tee -a "$LOG_FILE"
echo "       WI-FI NETWORK GRADE" | tee -a "$LOG_FILE"
echo "==========================================" | tee -a "$LOG_FILE"
echo "Status:           $HIFI_STATUS" | tee -a "$LOG_FILE"
echo "Idle Latency:     ${IDLE_AVG} ms" | tee -a "$LOG_FILE"
echo "Download Speed:   ${DL_SPEED}" | tee -a "$LOG_FILE"
echo "Download Bloat:   +${DL_BLOAT} ms" | tee -a "$LOG_FILE"
echo "Upload Speed:     ${UL_SPEED}" | tee -a "$LOG_FILE"
echo "Upload Bloat:     +${UL_BLOAT} ms" | tee -a "$LOG_FILE"
echo "------------------------------------------" | tee -a "$LOG_FILE"

# Grading
TOTAL_BLOAT=$(echo "$DL_BLOAT + $UL_BLOAT" | bc)
if (( $(echo "$TOTAL_BLOAT < 10" | bc -l) )); then
    GRADE="A+ (Excellent)"
elif (( $(echo "$TOTAL_BLOAT < 30" | bc -l) )); then
    GRADE="A (Great)"
elif (( $(echo "$TOTAL_BLOAT < 60" | bc -l) )); then
    GRADE="B (Good)"
elif (( $(echo "$TOTAL_BLOAT < 150" | bc -l) )); then
    GRADE="C (Fair)"
else
    GRADE="D/F (Poor)"
fi
echo "Bufferbloat Grade: $GRADE" | tee -a "$LOG_FILE"
echo "==========================================" | tee -a "$LOG_FILE"

# Cleanup
rm -f /tmp/ping_*.txt /tmp/speedtest_*.json
