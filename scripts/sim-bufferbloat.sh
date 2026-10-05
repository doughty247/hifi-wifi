#!/usr/bin/env bash
# Simulated bufferbloat test for autorate, using network namespaces (no Wi-Fi needed).
#
#   [client] cli0 <--> m0 [modem] m1 <--> isp0 [isp]   (reflector 10.9.9.9 on isp lo)
#
# The modem has a slow link with a deep, bloated buffer in both directions, like a typical
# cable/DSL modem. We saturate it with iperf3 and ping the reflector:
#   1) without hifi-wifi         -> latency under load = bufferbloat
#   2) with `hifi-wifi autorate` -> latency under load should stay near idle
#
# Usage: sudo scripts/sim-bufferbloat.sh [path/to/hifi-wifi]
#   DIRECTION=upload|download (default upload; download needs IFB + a tc classifier)
#   RATE=20mbit BUFFER=400ms DELAY=10ms LOAD_SECS=30
# Needs iproute2, iperf3, ping, python3. Uses netem for base delay if the kernel has it.
set -euo pipefail

BIN=$(realpath "${1:-target/release/hifi-wifi}")
DIRECTION=${DIRECTION:-upload}
RATE=${RATE:-20mbit}
BUFFER=${BUFFER:-400ms}
DELAY=${DELAY:-10ms}
LOAD_SECS=${LOAD_SECS:-30}
OUT=$(mktemp -d)

cleanup() {
    for ns in client modem isp; do
        ip netns pids "$ns" 2>/dev/null | xargs -r kill 2>/dev/null || true
    done
    sleep 0.5
    for ns in client modem isp; do ip netns del "$ns" 2>/dev/null || true; done
}
trap cleanup EXIT
cleanup

for ns in client modem isp; do ip netns add "$ns"; ip -n "$ns" link set lo up; done
ip link add cli0 netns client type veth peer name m0 netns modem
ip link add m1 netns modem type veth peer name isp0 netns isp

ip -n client addr add 10.0.0.2/24 dev cli0
ip -n modem addr add 10.0.0.1/24 dev m0
ip -n modem addr add 10.1.0.1/24 dev m1
ip -n isp addr add 10.1.0.2/24 dev isp0
ip -n isp addr add 10.9.9.9/32 dev lo
for l in "client cli0" "modem m0" "modem m1" "isp isp0"; do set -- $l; ip -n "$1" link set "$2" up; done
ip -n client route add default via 10.0.0.1
ip -n isp route add default via 10.1.0.1
ip -n modem route add 10.9.9.9/32 via 10.1.0.2
ip netns exec modem sysctl -qw net.ipv4.ip_forward=1

# Bloated bottleneck on the modem in both directions (m1 = upstream, m0 = downstream)
for dev in m1 m0; do
    if ip netns exec modem tc qdisc add dev "$dev" root handle 1: netem delay "$DELAY" limit 100000 2>/dev/null; then
        parent="parent 1: handle 2:"
    else
        parent="root"
        NO_NETEM=1
    fi
    ip netns exec modem tc qdisc add dev "$dev" $parent tbf rate "$RATE" burst 32kbit latency "$BUFFER"
done

ip netns exec isp iperf3 -s -D -B 10.9.9.9 >/dev/null
sleep 0.5

# Prints "idle_p50 load_p50 load_p95 mbit"
measure() {
    local tag=$1 reverse=""
    [[ "$DIRECTION" == download ]] && reverse="-R"
    ip netns exec client ping -n -i 0.2 -c 25 10.9.9.9 > "$OUT/$tag-idle.txt" || true
    ip netns exec client ping -n -i 0.2 -w "$LOAD_SECS" 10.9.9.9 > "$OUT/$tag-load.txt" &
    local pid=$!
    sleep 1
    ip netns exec client iperf3 -c 10.9.9.9 $reverse -P 4 -t $((LOAD_SECS - 3)) -J > "$OUT/$tag-iperf.json" || true
    wait $pid || true
    python3 - "$OUT/$tag-idle.txt" "$OUT/$tag-load.txt" "$OUT/$tag-iperf.json" <<'EOF'
import json, re, sys
def rtts(path, skip=0):
    return sorted(float(m) for m in re.findall(r"time=([\d.]+)", open(path).read())[skip:])
def pct(v, p): return v[min(len(v) - 1, int(len(v) * p))] if v else float("nan")
idle = rtts(sys.argv[1])
load = rtts(sys.argv[2], skip=30)  # ignore the first 6 s of load (autorate converging)
try:
    mbit = json.load(open(sys.argv[3]))["end"]["sum_received"]["bits_per_second"] / 1e6
except Exception:
    mbit = float("nan")
print(f"{pct(idle,.5):.1f} {pct(load,.5):.1f} {pct(load,.95):.1f} {mbit:.1f}")
EOF
}

echo "Bottleneck: $RATE $DIRECTION, buffer $BUFFER${NO_NETEM:+ (no netem: base RTT ~0)}"
read -r i1 l1 p1 m1 < <(measure baseline)

ip netns exec client "$BIN" autorate cli0 --always --reflector 10.9.9.9 > "$OUT/autorate.log" 2>&1 &
sleep 8   # idle probing builds the baseline
read -r i2 l2 p2 m2 < <(measure autorate)

echo
printf "%-12s %10s %12s %12s %12s\n" "" "idle p50" "loaded p50" "loaded p95" "throughput"
printf "%-12s %8sms %10sms %10sms %7s Mbit\n" "no shaping" "$i1" "$l1" "$p1" "$m1"
printf "%-12s %8sms %10sms %10sms %7s Mbit\n" "hifi-wifi" "$i2" "$l2" "$p2" "$m2"
echo
echo "Autorate log ($OUT/autorate.log):"
grep -v '^\s*$' "$OUT/autorate.log" | tail -n 8
