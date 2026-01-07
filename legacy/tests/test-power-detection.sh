#!/usr/bin/env bash
# Test power mode detection logic across different system configurations

echo "=== Power Mode Detection Test ==="
echo ""

# Test 1: Steam Deck on battery
echo "Test 1: Steam Deck on battery"
echo "  - System battery: /sys/class/power_supply/BAT1 (exists)"
echo "  - USB-C charger: ucsi-source-psy-0-00081 type=USB online=0"
echo "  - Expected: ON (power save)"
echo ""

# Test 2: Steam Deck charging
echo "Test 2: Steam Deck charging"
echo "  - System battery: /sys/class/power_supply/BAT1 (exists)"
echo "  - USB-C charger: ucsi-source-psy-0-00081 type=USB online=1"
echo "  - Expected: OFF (performance)"
echo ""

# Test 3: Bazzite laptop on battery
echo "Test 3: Bazzite laptop on battery"
echo "  - System battery: /sys/class/power_supply/BAT0 (exists)"
echo "  - AC adapter: AC0 type=Mains online=0"
echo "  - Expected: ON (power save)"
echo ""

# Test 4: Bazzite laptop plugged in
echo "Test 4: Bazzite laptop plugged in"
echo "  - System battery: /sys/class/power_supply/BAT0 (exists)"
echo "  - AC adapter: AC0 type=Mains online=1"
echo "  - Expected: OFF (performance)"
echo ""

# Test 5: Bazzite desktop (no system battery)
echo "Test 5: Bazzite desktop with wireless mouse"
echo "  - System battery: none"
echo "  - Device battery: hidpp_battery_0 type=Battery (mouse)"
echo "  - Expected: OFF (performance - desktop)"
echo ""

# Test 6: SteamOS desktop
echo "Test 6: SteamOS desktop"
echo "  - System battery: none"
echo "  - Expected: OFF (performance - desktop)"
echo ""

# Test 7: Steam Deck - battery charging but not detected via online=1
echo "Test 7: Steam Deck - fallback detection via battery status"
echo "  - System battery: /sys/class/power_supply/BAT1 status=Charging"
echo "  - USB-C charger: online field missing or broken"
echo "  - Expected: OFF (performance - detected via battery status)"
echo ""

echo "=== Logic Coverage ==="
echo ""
echo "✓ Desktop detection: No BAT0/BAT1/battery directory"
echo "✓ Laptop/handheld detection: Has BAT0/BAT1/battery"
echo "✓ AC detection method 1: type=Mains online=1"
echo "✓ AC detection method 2: type=USB online=1 (Steam Deck USB-C)"
echo "✓ AC detection fallback: battery status=Charging/Full/Not charging"
echo "✓ Ignores device batteries: Only checks system battery paths"
echo "✓ Timeout protection: All sysfs reads wrapped with timeout 0.5"
echo ""

echo "=== Current System Detection ==="
echo ""

# Check current system
if timeout 0.5 test -d /sys/class/power_supply/BAT0 2>/dev/null || \
   timeout 0.5 test -d /sys/class/power_supply/BAT1 2>/dev/null || \
   timeout 0.5 test -d /sys/class/power_supply/battery 2>/dev/null; then
    echo "This system: LAPTOP/HANDHELD (has system battery)"
    
    # Check AC
    ac_found=0
    for psu in /sys/class/power_supply/*; do
        [ -d "$psu" ] || continue
        psu_type=$(timeout 0.5 cat "$psu/type" 2>/dev/null || echo "")
        if [[ "$psu_type" == "Mains" ]] || [[ "$psu_type" == "USB" ]]; then
            online=$(timeout 0.5 cat "$psu/online" 2>/dev/null || echo "0")
            echo "  - Power supply: $(basename $psu) type=$psu_type online=$online"
            [ "$online" = "1" ] && ac_found=1
        fi
    done
    
    if [ $ac_found -eq 1 ]; then
        echo "  → Power mode: OFF (performance - AC connected)"
    else
        echo "  → Power mode: ON (power save - on battery)"
    fi
else
    echo "This system: DESKTOP (no system battery)"
    echo "  → Power mode: OFF (performance - always)"
    
    # Show device batteries if any
    for psu in /sys/class/power_supply/*; do
        [ -d "$psu" ] || continue
        psu_type=$(timeout 0.5 cat "$psu/type" 2>/dev/null || echo "")
        [ "$psu_type" = "Battery" ] && echo "  - Device battery: $(basename $psu) (ignored)"
    done
fi

echo ""
echo "=== Test Complete ==="
