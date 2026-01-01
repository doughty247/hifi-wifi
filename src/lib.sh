#!/bin/bash
# Common functions and variables for hifi-wifi

VERSION="1.3.0"

# Ensure homebrew binaries are in PATH (needed when running with sudo)
if [[ -d "/home/linuxbrew/.linuxbrew/bin" ]] && [[ ":$PATH:" != *":/home/linuxbrew/.linuxbrew/bin:"* ]]; then
    export PATH="/home/linuxbrew/.linuxbrew/bin:$PATH"
fi

# Configuration constants
STATE_DIR="/var/lib/wifi_patch"
LOGFILE="$STATE_DIR/auto-optimize.log"
BACKUP_PREFIX="$STATE_DIR/backup"
STATE_FLAG="$STATE_DIR/applied.flag"
FORCE_PERF_FLAG="$STATE_DIR/force_performance"
DEFAULT_BANDWIDTH="200mbit"
MIN_KERNEL_VERSION="5.15"

# Color codes (disable with --no-color)
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Helper functions for colored output
function log_info() {
    [[ ${NO_COLOR:-0} -eq 1 ]] && echo "[INFO] $*" || echo -e "${BLUE}[INFO]${NC} $*"
}

function log_success() {
    [[ ${NO_COLOR:-0} -eq 1 ]] && echo "[SUCCESS] $*" || echo -e "${GREEN}[SUCCESS]${NC} $*"
}

function log_warning() {
    [[ ${NO_COLOR:-0} -eq 1 ]] && echo "[WARNING] $*" || echo -e "${YELLOW}[WARNING]${NC} $*"
}

function log_error() {
    [[ ${NO_COLOR:-0} -eq 1 ]] && echo "[ERROR] $*" >&2 || echo -e "${RED}[ERROR]${NC} $*" >&2
}

# Check if network is idle, abort with clear message if busy
function check_network_idle_or_abort() {
  local ifc="$1"
  local threshold_kbps=300  # Consider busy if >300 KB/s traffic
  
  log_info "Checking network activity..."
  
  local rx1 tx1 rx2 tx2 rx_rate tx_rate total_rate
  rx1=$(cat "/sys/class/net/$ifc/statistics/rx_bytes" 2>/dev/null || echo 0)
  tx1=$(cat "/sys/class/net/$ifc/statistics/tx_bytes" 2>/dev/null || echo 0)
  sleep 2
  rx2=$(cat "/sys/class/net/$ifc/statistics/rx_bytes" 2>/dev/null || echo 0)
  tx2=$(cat "/sys/class/net/$ifc/statistics/tx_bytes" 2>/dev/null || echo 0)
  
  rx_rate=$(( (rx2 - rx1) / 2048 ))
  tx_rate=$(( (tx2 - tx1) / 2048 ))
  total_rate=$((rx_rate + tx_rate))
  
  if [[ $total_rate -gt $threshold_kbps ]]; then
    log_error "Network activity detected (${total_rate} KB/s)"
    log_error ""
    log_error "Common causes:"
    
    # Check for common bandwidth-consuming processes
    if pgrep -f "steam" >/dev/null 2>&1; then
      log_error "  - Steam downloads (Steam is running)"
    fi
    if pgrep -f "apt-get|apt|dnf|pacman|zypper" >/dev/null 2>&1; then
      log_error "  - System updates (package manager is running)"
    fi
    if pgrep -f "firefox|chrome|chromium" >/dev/null 2>&1; then
      log_error "  - Browser downloads"
    fi
    
    # Always show generic message if no specific processes found
    if ! pgrep -f "steam|apt|dnf|firefox|chrome" >/dev/null 2>&1; then
      log_error "  - Active downloads or updates"
      log_error "  - Background system processes"
    fi
    
    log_error ""
    log_error "Please close applications and pause downloads, then retry."
    log_error ""
    
    return 1
  fi
  
  log_success "Network is idle (${total_rate} KB/s) - safe to proceed"
  return 0
}

# Detect package manager and system type
function detect_package_manager() {
    local distro=""
    
    # Check if SteamOS/Bazzite
    if [[ -f /etc/os-release ]]; then
        local ID="" NAME="" ID_LIKE=""
        source /etc/os-release 2>/dev/null || true
        if [[ "$ID" == "steamos" ]] || [[ "${ID_LIKE:-}" =~ "steamos" ]]; then
            distro="steamos"
        elif [[ "$ID" == "bazzite" ]] || [[ "${NAME:-}" =~ "Bazzite" ]]; then
            distro="bazzite"
        elif [[ "${ID_LIKE:-}" =~ "fedora" ]]; then
            distro="fedora"
        fi
    fi
    
    # Return package manager command - prioritize brew for Bazzite
    # EXCEPTION: System components like iwd should use rpm-ostree if possible
    if command -v brew &>/dev/null && [[ "$distro" == "bazzite" ]]; then
        # Check if we are installing system components
        local is_system_component=0
        for arg in "$@"; do
            if [[ "$arg" == "iwd" ]]; then
                is_system_component=1
                break
            fi
        done
        
        if [[ $is_system_component -eq 1 ]] && command -v rpm-ostree &>/dev/null; then
             echo "rpm-ostree"
        else
             echo "brew"
        fi
    elif [[ "$distro" == "steamos" ]]; then
        echo "pacman"
    elif command -v rpm-ostree &>/dev/null; then
        echo "rpm-ostree"
    elif command -v dnf &>/dev/null; then
        echo "dnf"
    elif command -v apt &>/dev/null; then
        echo "apt"
    else
        echo "unknown"
    fi
}

# Install missing dependencies
function install_dependencies() {
    local missing_cmds=("$@")
    local pkg_mgr
    pkg_mgr=$(detect_package_manager "${missing_cmds[@]}")
    
    log_info "Detected package manager: $pkg_mgr"
    
    # Map commands to package names
    declare -A pkg_map=(
        ["ip"]="iproute2"
        ["nmcli"]="NetworkManager"
        ["iw"]="iw"
        ["tc"]="iproute2-tc"
        ["ethtool"]="ethtool"
        ["sysctl"]="procps-ng"
        ["iwd"]="iwd"
        ["bc"]="bc"
        ["curl"]="curl"
    )
    
    local packages=()
    for cmd in "${missing_cmds[@]}"; do
        if [[ -n "${pkg_map[$cmd]}" ]]; then
            packages+=("${pkg_map[$cmd]}")
        fi
    done
    
    # Remove duplicates
    packages=($(echo "${packages[@]}" | tr ' ' '\n' | sort -u))
    
    if [[ ${#packages[@]} -eq 0 ]]; then
        return 0
    fi
    
    log_warning "Missing packages: ${packages[*]}"
    
    case "$pkg_mgr" in
        brew)
            log_info "Installing via Homebrew..."
            brew install "${packages[@]}" || {
                log_error "Failed to install via brew. Try: brew install ${packages[*]}"
                return 1
            }
            ;;
        rpm-ostree)
            log_info "Installing via rpm-ostree (requires reboot after)..."
            log_warning "Note: rpm-ostree requires a system reboot to apply packages"
            read -p "Install packages now? [y/N]: " -n 1 -r
            echo
            if [[ $REPLY =~ ^[Yy]$ ]]; then
                rpm-ostree install --apply-live "${packages[@]}" 2>/dev/null || \
                rpm-ostree install "${packages[@]}" || {
                    log_error "Failed to install via rpm-ostree"
                    return 1
                }
                log_success "Packages layered. May need reboot for full effect."
            else
                log_error "User cancelled installation"
                return 1
            fi
            ;;
        pacman)
            log_info "Installing via pacman..."
            
            # Create cache directory for persistence
            local cache_dir="/var/lib/hifi-wifi/pkg_cache"
            sudo mkdir -p "$cache_dir"
            
            # Download packages to cache first (so we can restore them after update)
            log_info "Caching packages for persistence..."
            sudo pacman -Sw --noconfirm --cachedir "$cache_dir" "${packages[@]}" || true
            
            # Install from cache if possible, or normal install
            # We use -U with wildcard because version numbers might change
            if ls "$cache_dir"/*.pkg.tar.zst 1> /dev/null 2>&1; then
                 sudo pacman -U --noconfirm "$cache_dir"/*.pkg.tar.zst || sudo pacman -S --noconfirm "${packages[@]}" || return 1
            else
                 sudo pacman -S --noconfirm "${packages[@]}" || return 1
            fi
            ;;
        dnf)
            log_info "Installing via dnf..."
            sudo dnf install -y "${packages[@]}" || return 1
            ;;
        apt)
            log_info "Installing via apt..."
            sudo apt update && sudo apt install -y "${packages[@]}" || return 1
            ;;
        *)
            log_error "Unsupported package manager. Please install: ${packages[*]}"
            return 1
            ;;
    esac
}

# Check for required commands
function check_dependencies() {
    local missing_deps=()
    local required_cmds=("ip" "nmcli" "iw" "tc" "ethtool" "sysctl" "bc")
    
    for cmd in "${required_cmds[@]}"; do
        if ! command -v "$cmd" &>/dev/null; then
            missing_deps+=("$cmd")
        fi
    done
    
    if [[ ${#missing_deps[@]} -gt 0 ]]; then
        log_error "Missing required commands: ${missing_deps[*]}"
        
        # Offer to install automatically
        if [[ $EUID -eq 0 ]] || [[ -n "$SUDO_USER" ]]; then
            log_info "Attempting automatic installation..."
            if install_dependencies "${missing_deps[@]}"; then
                log_success "All dependencies installed!"
                return 0
            else
                log_error "Automatic installation failed"
                return 1
            fi
        else
            log_error "Re-run with sudo to auto-install dependencies"
            return 1
        fi
    fi
    return 0
}

# Detect the interface that is actually carrying traffic (default route)
function detect_default_interface() {
    # Get the interface used for the default route (this is where traffic actually flows)
    local default_ifc
    default_ifc=$(ip route show default 2>/dev/null | head -1 | awk '{print $5}')
    
    if [[ -n "$default_ifc" ]] && ip link show "$default_ifc" &>/dev/null; then
        echo "$default_ifc"
        return 0
    fi
    
    # Fallback: try IPv6 default route
    default_ifc=$(ip -6 route show default 2>/dev/null | head -1 | awk '{print $5}')
    if [[ -n "$default_ifc" ]] && ip link show "$default_ifc" &>/dev/null; then
        echo "$default_ifc"
        return 0
    fi
    
    return 1
}

# Detect interface type (wifi, ethernet, etc)
function get_interface_type() {
    local ifc="$1"
    if [[ "$ifc" =~ ^wl ]]; then
        echo "wifi"
    elif [[ "$ifc" =~ ^(en|eth) ]]; then
        echo "ethernet"
    elif [[ "$ifc" =~ ^(tailscale|tun|tap) ]]; then
        echo "vpn"
    else
        echo "unknown"
    fi
}

function detect_interface() {
    if [[ -n "$INTERFACE" ]]; then
        if ip link show "$INTERFACE" &>/dev/null; then
            echo "$INTERFACE"
            return 0
        else
            log_error "Specified interface '$INTERFACE' not found"
            return 1
        fi
    fi
    
    # PRIORITY 1: Use the interface that's actually carrying traffic (default route)
    local default_ifc
    if default_ifc=$(detect_default_interface); then
        local ifc_type=$(get_interface_type "$default_ifc")
        # Skip VPN interfaces, we want the underlying connection
        if [[ "$ifc_type" != "vpn" ]]; then
            # Send log to stderr so it doesn't pollute the return value
            echo "[INFO] Detected active interface: $default_ifc ($ifc_type)" >&2
            echo "$default_ifc"
            return 0
        fi
    fi
    
    # PRIORITY 2: Pick first wireless interface in state UP
    local ifc
    ifc=$(ip -o link show | awk -F': ' '{print $2}' | grep -E '^wl' | head -n1)
    
    # PRIORITY 3: If no wireless, look for ethernet (en* or eth*) that is UP
    if [[ -z "$ifc" ]]; then
        ifc=$(ip -o link show | awk -F': ' '{print $2}' | grep -E '^(en|eth)' | head -n1)
    fi
    
    if [[ -z "$ifc" ]]; then
        log_warning "No network interface found"
        return 1
    fi
    
    echo "$ifc"
    return 0
}

function backup_connection() {
    local uuid="$1"
    [[ -z "$uuid" ]] && return 0
    local file="$BACKUP_PREFIX-$uuid.txt"
    
    if [[ ${DRY_RUN:-0} -eq 1 ]]; then
        log_info "[DRY-RUN] Would backup connection $uuid to $file"
        return 0
    fi
    
    if [[ -f "$file" ]]; then
        log_info "Existing backup for $uuid (skipping)"
    else
        log_info "Saving original settings for connection $uuid"
        if nmcli connection show "$uuid" > "$file" 2>/dev/null; then
            log_success "Backup created: $file"
        else
            log_error "Failed to create backup for $uuid"
            return 1
        fi
    fi
    return 0
}

function restore_connection() {
    local uuid="$1"
    local file="$BACKUP_PREFIX-$uuid.txt"
    if [[ ! -f "$file" ]]; then
        echo "[RESTORE] No backup found for $uuid" >&2
        return 0
    fi
    
    # Check if connection still exists
    if ! nmcli connection show "$uuid" &>/dev/null; then
        echo "[RESTORE] Connection $uuid not found (may have been deleted). Skipping restore."
        return 0
    fi

    echo "[RESTORE] Restoring core fields for connection $uuid"
    # Restore ipv6.method
    local ipv6_method=$(grep '^ipv6.method:' "$file" | awk '{print $2}')
    if [[ -n "$ipv6_method" ]]; then
        nmcli connection modify "$uuid" ipv6.method "$ipv6_method"
    fi
    # Restore cloned mac (may be -- OR permanent). If random then set to random
    local mac=$(grep '^wifi.cloned-mac-address:' "$file" | awk '{print $2}')
    if [[ -n "$mac" ]]; then
        nmcli connection modify "$uuid" wifi.cloned-mac-address "$mac"
    else
        nmcli connection modify "$uuid" wifi.cloned-mac-address "" || true
    fi
}

function current_ssid_uuid() {
    # Try to get UUID of active connection on the detected interface
    local ifc
    ifc=$(detect_interface) || return 0
    
    local uuid=$(nmcli -t -f UUID,DEVICE connection show --active | grep ":$ifc" | cut -d: -f1 | head -1)
    
    if [[ -z "$uuid" ]]; then
        # Fallback: try to match by SSID if wifi
        local ssid=$(nmcli -t -f active,ssid dev wifi 2>/dev/null | awk -F: '$1=="yes" {print $2; exit}')
        if [[ -n "$ssid" ]]; then
            uuid=$(nmcli -t -f uuid,name connection show | awk -F: -v s="$ssid" '$2==s {print $1; exit}')
        fi
    fi
    echo "$uuid"
}

function get_current_ssid() {
    local ssid=$(nmcli -t -f active,ssid dev wifi 2>/dev/null | awk -F: '$1=="yes" {print $2; exit}')
    if [[ -n "$ssid" ]]; then
        echo "$ssid"
    else
        # Fallback to active connection name
        nmcli -t -f NAME,TYPE connection show --active | grep -vE ":(bridge|lo|docker)" | head -1 | cut -d: -f1
    fi
}

function sanitize_ssid() {
    # Convert SSID to safe filename
    echo "$1" | tr -cd '[:alnum:]_-' | tr '[:upper:]' '[:lower:]'
}

# NOTE: Network profile caching removed in v1.3.0
# Link Statistics are instant and always accurate, so we measure fresh on every connection.
# CAKE always gets the correct bandwidth limit - no stale data possible.

function enable_iwd() {
    log_info "Switching to iwd backend (--use-iwd was specified)..."
    
    # Warn SteamOS users about potential Developer Options conflict
    if [[ -f /etc/os-release ]]; then
        source /etc/os-release
        if [[ "$ID" == "steamos" ]] || [[ "${ID_LIKE:-}" =~ "steamos" ]]; then
            log_warning "SteamOS detected! iwd may conflict with Developer Options."
            log_warning "If WiFi breaks, disable 'Force WPA Supplicant' in Developer Options."
        fi
    fi
    
    # Check for iwd presence
    local iwd_installed=false
    if command -v iwd &>/dev/null || \
       [[ -x /usr/libexec/iwd ]] || \
       [[ -x /usr/lib/iwd/iwd ]] || \
       systemctl list-unit-files iwd.service &>/dev/null; then
        iwd_installed=true
    fi

    if [ "$iwd_installed" = false ]; then
        log_warning "iwd not found. Installing..."
        install_dependencies "iwd" || {
            log_error "Could not install iwd. Skipping backend switch."
            return 1
        }
    fi

    log_info "Testing iwd compatibility with your hardware..."
    
    # Start iwd temporarily to test
    systemctl start iwd.service 2>/dev/null
    
    # Wait for service to stabilize and check if it actually started
    sleep 3
    if ! systemctl is-active --quiet iwd.service; then
        log_error "iwd service failed to start. Your hardware may not be supported."
        log_info "Keeping wpa_supplicant as backend."
        return 1
    fi
    
    # Check if iwd can see the Wi-Fi adapter via D-Bus (more reliable than iwctl)
    local iwd_devices_found=false
    if command -v busctl &>/dev/null; then
        if busctl tree net.connman.iwd 2>/dev/null | grep -q "net/connman/iwd"; then
            iwd_devices_found=true
        fi
    else
        # Fallback: check sysfs for phy devices
        if ls /sys/class/ieee80211/phy* &>/dev/null; then
            iwd_devices_found=true
        fi
    fi
    
    if [ "$iwd_devices_found" = false ]; then
        log_error "iwd started but cannot detect your Wi-Fi hardware."
        log_warning "This could indicate driver incompatibility."
        log_info "Keeping wpa_supplicant as backend for safety."
        systemctl stop iwd.service 2>/dev/null
        return 1
    fi

    log_success "iwd successfully detected your Wi-Fi hardware!"
    log_info "Switching NetworkManager backend to iwd..."
    
    # Create configuration to switch backend
    mkdir -p /etc/NetworkManager/conf.d
    create_tracked_file /etc/NetworkManager/conf.d/wifi_backend.conf <<EOF
[device]
wifi.backend=iwd
EOF

    # Mask wpa_supplicant to prevent conflicts
    systemctl mask wpa_supplicant.service 2>/dev/null || true
    systemctl stop wpa_supplicant.service 2>/dev/null || true

    # Enable iwd to start on boot
    systemctl enable iwd.service 2>/dev/null || true

    # Give iwd a moment to fully initialize D-Bus interfaces before NetworkManager restarts
    sleep 2

    log_success "Switched to iwd backend. NetworkManager restart required."
}

# Comprehensive Wi-Fi card detection function
function detect_wifi_hardware() {
    local driver=""
    local vendor=""
    local device_id=""
    local device_name=""
    local bus_type=""
    
    # Method 1: Get interface and query ethtool for driver
    local iface
    iface=$(detect_interface 2>/dev/null)
    if [[ -n "$iface" ]]; then
        driver=$(ethtool -i "$iface" 2>/dev/null | grep "^driver:" | awk '{print $2}')
        if [[ -n "$driver" ]]; then
            log_info "Driver detected via ethtool: $driver"
        fi
    fi
    
    # Method 2: Check loaded kernel modules for common Wi-Fi drivers
    if [[ -z "$driver" ]]; then
        local wifi_modules
        wifi_modules=$(lsmod | grep -E '^(rtw89|rtw88|rtl|mt76|mt7|iwl|ath|brcm|carl|zd|p54|libertas|mwl|wl)' | awk '{print $1}')
        if [[ -n "$wifi_modules" ]]; then
            driver=$(echo "$wifi_modules" | head -1)
            log_info "Driver detected via lsmod: $driver"
        fi
    fi
    
    # Method 3: Parse lspci for PCI Wi-Fi cards
    local pci_info
    pci_info=$(lspci -k | grep -A 5 -i "network\|wireless\|wi-fi\|802\.11")
    if [[ -n "$pci_info" ]]; then
        bus_type="PCI"
        vendor=$(echo "$pci_info" | grep -oE "\[[0-9a-f]{4}:[0-9a-f]{4}\]" | head -1 | tr -d '[]')
        device_name=$(echo "$pci_info" | grep -E "Network controller|Wireless" | head -1 | sed 's/.*: //')
        
        # Extract driver from kernel driver line
        if [[ -z "$driver" ]]; then
            driver=$(echo "$pci_info" | grep "Kernel driver in use:" | head -1 | awk -F': ' '{print $2}' | tr -d ' ')
        fi
    fi
    
    # Method 4: Check USB Wi-Fi adapters
    if [[ -z "$driver" ]] || [[ -z "$bus_type" ]]; then
        local usb_info
        usb_info=$(lsusb | grep -iE "wireless|wi-fi|802\.11|wlan|atheros|realtek|ralink|mediatek|intel")
        if [[ -n "$usb_info" ]]; then
            bus_type="USB"
            device_name=$(echo "$usb_info" | head -1 | sed 's/.*ID [0-9a-f]*:[0-9a-f]* //')
            
            # Try to find USB driver
            if [[ -z "$driver" ]] && [[ -n "$iface" ]]; then
                local usb_driver
                usb_driver=$(readlink "/sys/class/net/$iface/device/driver" 2>/dev/null | xargs basename)
                [[ -n "$usb_driver" ]] && driver="$usb_driver"
            fi
        fi
    fi
    
    # Method 5: Check /sys/class/net for driver info
    if [[ -z "$driver" ]] && [[ -n "$iface" ]]; then
        if [[ -L "/sys/class/net/$iface/device/driver" ]]; then
            driver=$(readlink "/sys/class/net/$iface/device/driver" | xargs basename)
        fi
    fi
    
    # Export detected information
    DETECTED_DRIVER="$driver"
    WIFI_VENDOR="$vendor"
    WIFI_DEVICE="$device_name"
    WIFI_BUS="$bus_type"
    
    if [[ -n "$DETECTED_DRIVER" ]]; then
        log_info "Wi-Fi Hardware Detection:"
        log_info "  Driver: $DETECTED_DRIVER"
        [[ -n "$WIFI_BUS" ]] && log_info "  Bus: $WIFI_BUS"
        [[ -n "$WIFI_DEVICE" ]] && log_info "  Device: $WIFI_DEVICE"
        [[ -n "$WIFI_VENDOR" ]] && log_info "  Vendor:Device: $WIFI_VENDOR"
        return 0
    else
        log_warning "Could not auto-detect Wi-Fi driver"
        return 1
    fi
}

# Detect all active network interfaces (Ethernet and Wi-Fi)
function detect_all_interfaces() {
    # Find all interfaces that are UP and not loopback/vpn
    # We look for 'state UP' in ip link output
    ip -o link show up | awk -F': ' '{print $2}' | grep -E '^(en|eth|wl)' | sort -u
}
