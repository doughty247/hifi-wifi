#!/bin/bash
set -e

SERVER=$1

if [ -z "$SERVER" ]; then
    echo "Usage: $0 <server_ip>"
    echo "Please provide the IP of your iperf3 server."
    exit 1
fi

# Ensure hifi-wifi is installed or available in path
HIFI_CMD="hifi-wifi"
if ! command -v hifi-wifi &> /dev/null; then
    if [ -f "./target/release/hifi-wifi" ]; then
        echo "Warning: hifi-wifi not in PATH, using local binary..."
        HIFI_CMD="./target/release/hifi-wifi"
    else
        echo "Error: hifi-wifi command not found and ./target/release/hifi-wifi missing."
        echo "Please install or build first."
        exit 1
    fi
fi

if [ ! -f "/etc/systemd/system/hifi-wifi.service" ]; then
    echo "Warning: hifi-wifi service not found. 'hifi-wifi on' requires the service."
    echo "If the test fails, try running: sudo $HIFI_CMD install"
fi

TIMESTAMP=$(date +%Y%m%d_%H%M%S)
export TIMESTAMP

echo "=== HIFI-WIFI A/B AUTO-TESTER ==="
echo "Session ID: $TIMESTAMP"
echo "Server: $SERVER"
echo "---------------------------------"

# 1. STOCK RUN
echo ""
echo ">>> STEP 1: Configuring STOCK environment (hifi-wifi off)..."
sudo $HIFI_CMD off
echo "Waiting 8 seconds for network to stabilize..."
sleep 8

echo ">>> Running STOCK benchmarks..."
# Pass timestamp via env var so run_benchmarks.sh uses it
./tests/run_benchmarks.sh "$SERVER" stock

# 2. HIFI RUN
echo ""
echo ">>> STEP 2: Configuring HIFI environment (hifi-wifi on)..."
sudo $HIFI_CMD on
echo "Waiting 8 seconds for network to stabilize..."
sleep 8

echo ">>> Running HIFI benchmarks..."
./tests/run_benchmarks.sh "$SERVER" hifi

# 3. GRADE
echo ""
echo ">>> STEP 3: Grading Results..."
STOCK_DIR="tests/results/stock_${TIMESTAMP}"
HIFI_DIR="tests/results/hifi_${TIMESTAMP}"

if [ -d "$STOCK_DIR" ] && [ -d "$HIFI_DIR" ]; then
    python3 tests/grader.py "$STOCK_DIR" "$HIFI_DIR"
else
    echo "Error: Result directories not found."
    ls -l tests/results/
fi
