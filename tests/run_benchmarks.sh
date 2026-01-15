#!/bin/bash
# hifi-wifi A/B Benchmark Suite
# Usage: ./tests/run_benchmarks.sh <target_server> [test_label]

SERVER="${1}"
TEST_LABEL="${2:-test}"
# Allow overriding TIMESTAMP from environment for grouped runs
TIMESTAMP="${TIMESTAMP:-$(date +%Y%m%d_%H%M%S)}"
OUTPUT_DIR="tests/results/${TEST_LABEL}_${TIMESTAMP}"
mkdir -p "$OUTPUT_DIR"

if [ -z "$SERVER" ]; then
    echo "Usage: $0 <target_server_ip> [test_label]"
    echo "  <target_server_ip>: IP address of a machine running 'iperf3 -s' (local preferred)"
    echo "  [test_label]: Optional label (e.g. 'stock' or 'hifi')"
    echo ""
    echo "Example: $0 192.168.1.5 stock"
    exit 1
fi

echo "=== hifi-wifi Benchmark: $TEST_LABEL ==="
echo "Target: $SERVER"
echo "Output: $OUTPUT_DIR"
echo "Timestamp: $TIMESTAMP"

# 1. Metadata
echo "Collecting system metadata..."
{
    echo "{"
    echo "  \"timestamp\": \"$TIMESTAMP\","
    echo "  \"label\": \"$TEST_LABEL\","
    echo "  \"kernel\": \"$(uname -r)\","
    echo "  \"hifi_status\": \"$(systemctl is-active hifi-wifi)\""
    echo "}"
} > "$OUTPUT_DIR/meta.json"

# 2. WiFi Stats (Before)
echo "Collecting WiFi stats..."
iw dev | grep Interface | awk '{print $2}' | xargs -I {} iw dev {} station dump > "$OUTPUT_DIR/wifi_stats.txt"

# 3. Latency (MTR)
echo "[1/4] Running MTR latency/loss analysis (JSON)..."
if command -v mtr &> /dev/null; then
    # -r: report, -c 50: 50 cycles, -n: no dns, -j: json
    sudo mtr -r -c 50 -n -j "$SERVER" > "$OUTPUT_DIR/mtr_latency.json"
else
    echo "  Skipping MTR (not found)"
fi

# 4. Throughput (iperf3 TCP)
echo "[2/4] Running iperf3 TCP throughput (30s)..."
if command -v iperf3 &> /dev/null; then
    iperf3 -c "$SERVER" -t 30 -J > "$OUTPUT_DIR/iperf3_tcp.json"
else
    echo "  Skipping iperf3 (not found)"
fi

# 5. Jitter/Loss (iperf3 UDP)
echo "[3/4] Running iperf3 UDP jitter/bufferbloat check (30s)..."
if command -v iperf3 &> /dev/null; then
    # -u: UDP, -b 100M: limit to 100M to test handling, -R: reverse (download) is often more interesting for gaming
    iperf3 -c "$SERVER" -u -b 100M -t 30 -J > "$OUTPUT_DIR/iperf3_udp.json"
fi

# 6. Flent (Bufferbloat) - Optional
if command -v flent &> /dev/null; then
    echo "[4/4] Running Flent bufferbloat test..."
    flent rrul -H "$SERVER" -l 60 --format=json -o "$OUTPUT_DIR/flent_rrul.json"
else
    echo "[4/4] Skipping Flent (not installed)"
fi

echo ""
echo "=== Test Complete ==="
echo "Results saved in $OUTPUT_DIR"

# Quick Summary extraction if jq is available
if command -v jq &> /dev/null && [ -s "$OUTPUT_DIR/iperf3_tcp.json" ]; then
    # Check if iperf3 run was successful
    if jq -e '.end.sum_received.bits_per_second' "$OUTPUT_DIR/iperf3_tcp.json" > /dev/null; then
        echo "Quick Stats:"
        TCP_SPEED=$(jq -r '.end.sum_received.bits_per_second / 1000000 | floor' "$OUTPUT_DIR/iperf3_tcp.json")
        echo "  TCP Throughput: ${TCP_SPEED} Mbps"
        
        if [ -s "$OUTPUT_DIR/iperf3_udp.json" ] && jq -e '.end.sum.jitter_ms' "$OUTPUT_DIR/iperf3_udp.json" > /dev/null; then
            JITTER=$(jq -r '.end.sum.jitter_ms' "$OUTPUT_DIR/iperf3_udp.json")
            LOSS=$(jq -r '.end.sum.lost_percent' "$OUTPUT_DIR/iperf3_udp.json")
            echo "  UDP Jitter: ${JITTER} ms"
            echo "  UDP Loss: ${LOSS}%"
        fi
        
        if [ -s "$OUTPUT_DIR/mtr_latency.json" ]; then
            AVG_LATENCY=$(jq -r '.report.hubs[-1].Avg // 0' "$OUTPUT_DIR/mtr_latency.json") 
            echo "  Avg Latency: ${AVG_LATENCY} ms"
        fi
    else
        echo "Test failed or target unreachable (check iperf3 logs in output dir)"
    fi
fi
