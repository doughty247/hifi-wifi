#!/bin/bash
# realtime-streaming-monitor.sh
# Monitor network quality DURING Steam Remote Play session
# Uses mtr for accurate real-time packet loss detection

INTERFACE=$(ip route | grep default | awk '{print $5}' | head -1)
LOG_FILE="streaming_quality_$(date +%Y%m%d_%H%M%S).log"
TEST_HOST="8.8.8.8"  # Google DNS for latency testing

echo "=========================================="
echo "STEAM REMOTE PLAY NETWORK MONITOR"
echo "=========================================="
echo "Interface: $INTERFACE"
echo "Test host: $TEST_HOST"
echo "Log file: $LOG_FILE"
echo ""
echo "Monitoring network during streaming session..."
echo "Press Ctrl+C to stop"
echo "=========================================="
echo ""

# Write header to log
{
    echo "Steam Remote Play Network Quality Monitor"
    echo "Started: $(date)"
    echo "Interface: $INTERFACE"
    echo "Test Host: $TEST_HOST"
    echo "=========================================="
    echo ""
} > "$LOG_FILE"

# Counter for samples
SAMPLE=0

while true; do
    SAMPLE=$((SAMPLE + 1))
    
    {
        echo "=== SAMPLE #$SAMPLE | $(date +%H:%M:%S) ==="
        
        # Use mtr for accurate packet loss and latency measurement
        # Send 5 packets (5 seconds with default 1-sec interval, no root needed)
        MTR_OUTPUT=$(mtr --report --report-cycles 5 --no-dns "$TEST_HOST" 2>/dev/null | tail -1)
        
        if [[ -n "$MTR_OUTPUT" ]]; then
            # Parse mtr output: Host Loss% Snt Last Avg Best Wrst StDev
            LOSS=$(echo "$MTR_OUTPUT" | awk '{print $3}' | tr -d '%')
            AVG_LATENCY=$(echo "$MTR_OUTPUT" | awk '{print $6}')
            BEST_LATENCY=$(echo "$MTR_OUTPUT" | awk '{print $7}')
            WORST_LATENCY=$(echo "$MTR_OUTPUT" | awk '{print $8}')
            JITTER=$(echo "$MTR_OUTPUT" | awk '{print $9}')
            
            echo "Latency: avg=${AVG_LATENCY}ms best=${BEST_LATENCY}ms worst=${WORST_LATENCY}ms jitter=${JITTER}ms"
            echo "Packet Loss: ${LOSS}%"
            
            # Warn if packet loss detected (simple string comparison)
            if [[ -n "$LOSS" ]] && [[ "$LOSS" != "0.0" ]] && [[ "$LOSS" != "0" ]]; then
                echo "WARNING: Packet loss detected!"
            fi
        else
            echo "Latency: ERROR - Unable to reach $TEST_HOST"
            echo "Packet Loss: N/A"
        fi
        
        # Current bandwidth usage
        RX_BYTES=$(cat /sys/class/net/$INTERFACE/statistics/rx_bytes 2>/dev/null || echo 0)
        TX_BYTES=$(cat /sys/class/net/$INTERFACE/statistics/tx_bytes 2>/dev/null || echo 0)
        sleep 1
        RX_BYTES_NEW=$(cat /sys/class/net/$INTERFACE/statistics/rx_bytes 2>/dev/null || echo 0)
        TX_BYTES_NEW=$(cat /sys/class/net/$INTERFACE/statistics/tx_bytes 2>/dev/null || echo 0)
        
        RX_RATE=$(( (RX_BYTES_NEW - RX_BYTES) / 1024 ))  # KB/s
        TX_RATE=$(( (TX_BYTES_NEW - TX_BYTES) / 1024 ))  # KB/s
        
        echo "Bandwidth: Download ${RX_RATE} KB/s | Upload ${TX_RATE} KB/s"
        
        echo ""
        
    } | tee -a "$LOG_FILE"
    
    sleep 1
done
