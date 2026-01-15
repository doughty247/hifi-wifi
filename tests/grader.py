#!/usr/bin/env python3
import sys
import json
import os
from pathlib import Path

def load_json(path):
    try:
        with open(path) as f:
            return json.load(f)
    except FileNotFoundError:
        return None
    except json.JSONDecodeError:
        return None

def get_iperf_tcp_speed(data):
    if not data: return None
    # keys: end -> sum_received -> bits_per_second
    try:
        return data['end']['sum_received']['bits_per_second'] / 1e6 # Mbps
    except (KeyError, TypeError):
        return None

def get_iperf_udp_stats(data):
    if not data: return (None, None)
    # jitter_ms, lost_percent
    try:
        # Check if error field exists (iperf failed)
        if 'error' in data:
            return (None, None)

        # For UDP, stats are in sum or streams
        sum_data = data['end']['sum']
        jitter = sum_data.get('jitter_ms', 0.0)
        lost_percent = sum_data.get('lost_percent', 0.0)
        return float(jitter), float(lost_percent)
    except (KeyError, TypeError):
        return (None, None)

def get_mtr_stats(data):
    if not data: return (None, None)
    try:
        # report -> hubs -> last one
        report = data.get('report', {})
        hubs = report.get('hubs', [])
        
        # Some versions output report -> hub (singular list) or just report -> hubs
        if not hubs:
             # Try finding it recursively or handle empty
             return (None, None)
            
        last_hop = hubs[-1]
        loss = last_hop.get('Loss%', 0.0)
        avg = last_hop.get('Avg', 0.0)
        
        # If last hop has 100% loss, it's effectively infinite latency or timeout
        # But we return it as is, and handle display logic
        return float(avg), float(loss)
    except (KeyError, IndexError, TypeError):
        return (None, None)

def color(text, color_code):
    if sys.stdout.isatty():
        return f"\033[{color_code}m{text}\033[0m"
    return text

def red(t): return color(t, "31")
def green(t): return color(t, "32")
def bold(t): return color(t, "1")
def yellow(t): return color(t, "33")

def fmt_val(val, unit="", width=10, decimals=2):
    if val is None:
        return "       N/A".ljust(width)
    return f"{val:{width}.{decimals}f}"

def main():
    if len(sys.argv) < 3:
        print("Usage: grader.py <stock_dir> <hifi_dir>")
        sys.exit(1)
        
    stock_dir = Path(sys.argv[1])
    hifi_dir = Path(sys.argv[2])
    
    print(bold(f"\n=== HIFI-WIFI BENCHMARK GRADER ==="))
    print(f"Stock: {stock_dir.name}")
    print(f"HiFi:  {hifi_dir.name}")
    print("-" * 75)
    print(f"{'METRIC':<25} | {'STOCK':<12} | {'HIFI':<12} | {'DIFF':<10}")
    print("-" * 75)
    
    # 1. TCP Throughput
    stock_tcp = load_json(stock_dir / 'iperf_tcp.json')
    hifi_tcp = load_json(hifi_dir / 'iperf_tcp.json')
    
    s_mbps = get_iperf_tcp_speed(stock_tcp)
    h_mbps = get_iperf_tcp_speed(hifi_tcp)
    
    diff_str = "    --    "
    if s_mbps is not None and h_mbps is not None and s_mbps > 0:
        diff_tcp = ((h_mbps - s_mbps) / s_mbps * 100)
        diff_str = f"{diff_tcp:+7.1f}%"
        if diff_tcp > 5: diff_str = green(diff_str)
        elif diff_tcp < -5: diff_str = red(diff_str)
    
    print(f"{'TCP Speed (Mbps)':<25} | {fmt_val(s_mbps):<12} | {fmt_val(h_mbps):<12} | {diff_str}")

    # 2. UDP Jitter
    stock_udp = load_json(stock_dir / 'iperf_udp.json')
    hifi_udp = load_json(hifi_dir / 'iperf_udp.json')
    
    s_jit, s_loss = get_iperf_udp_stats(stock_udp)
    h_jit, h_loss = get_iperf_udp_stats(hifi_udp)
    
    diff_str = "    --    "
    if s_jit is not None and h_jit is not None and s_jit > 0:
        diff_jit = ((h_jit - s_jit) / s_jit * 100)
        diff_str = f"{diff_jit:+7.1f}%"
        if diff_jit < -10: diff_str = green(diff_str)
        elif diff_jit > 10: diff_str = red(diff_str)
    
    print(f"{'UDP Jitter (ms)':<25} | {fmt_val(s_jit):<12} | {fmt_val(h_jit):<12} | {diff_str}")
    
    # 3. UDP Loss
    diff_str = "    --    "
    if s_loss is not None and h_loss is not None:
        diff_loss = h_loss - s_loss
        diff_str = f"{diff_loss:+7.2f}"
        if diff_loss < 0: diff_str = green(diff_str)
        elif diff_loss > 0: diff_str = red(diff_str)

    print(f"{'UDP Loss (%)':<25} | {fmt_val(s_loss):<12} | {fmt_val(h_loss):<12} | {diff_str}")

    # 4. MTR Latency
    stock_mtr = load_json(stock_dir / 'mtr_latency.json')
    hifi_mtr = load_json(hifi_dir / 'mtr_latency.json')
    
    s_lat, s_mloss = get_mtr_stats(stock_mtr)
    h_lat, h_mloss = get_mtr_stats(hifi_mtr)
    
    # Handle MTR 100% loss (timeout)
    s_lat_disp = fmt_val(s_lat)
    h_lat_disp = fmt_val(h_lat)
    
    if s_mloss == 100.0: s_lat_disp = "   Timeout  "
    if h_mloss == 100.0: h_lat_disp = "   Timeout  "

    lat_str = "    --    "
    if s_lat is not None and h_lat is not None and s_lat > 0 and s_mloss < 100 and h_mloss < 100:
        diff_lat = ((h_lat - s_lat) / s_lat * 100)
        lat_str = f"{diff_lat:+7.1f}%"
        if diff_lat < -5: lat_str = green(lat_str)
        elif diff_lat > 5: lat_str = red(lat_str)
    
    print(f"{'MTR Latency (avg ms)':<25} | {s_lat_disp:<12} | {h_lat_disp:<12} | {lat_str}")
    
    if s_mloss is not None and (s_mloss > 0 or h_mloss > 0):
         # Print MTR Loss if meaningful
         print(f"{'MTR Pkt Loss (%)':<25} | {fmt_val(s_mloss):<12} | {fmt_val(h_mloss):<12} | {'--':>10}")

    print("-" * 75)
    
    # Overall Assessment
    if s_mbps is None and h_mbps is None and s_lat is None:
        print(yellow("NO DATA COLLECTED (Check connection or tools)"))
        return

    score = 0
    # Only score if data exists
    if s_mbps and h_mbps:
        diff = ((h_mbps - s_mbps) / s_mbps * 100) if s_mbps > 0 else 0
        if diff > 5: score += 1
        elif diff < -5: score -= 1
        
    print("Assessment: ", end="")
    if score > 0:
        print(green("IMPROVEMENT DETECTED ✅"))
    elif score < 0:
        print(red("POSSIBLE REGRESSION ❌"))
    else:
        print("NO SIGNIFICANT CHANGE ➖")
    print("\n")

if __name__ == "__main__":
    main()
