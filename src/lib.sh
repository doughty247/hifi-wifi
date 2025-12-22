#!/bin/bash
# Common functions and variables for hifi-wifi

VERSION="1.1.0"

# Configuration constants
STATE_DIR="/var/lib/wifi_patch"
LOGFILE="$STATE_DIR/auto-optimize.log"
BACKUP_PREFIX="$STATE_DIR/backup"
STATE_FLAG="$STATE_DIR/applied.flag"
FORCE_PERF_FLAG="$STATE_DIR/force_performance"
NETWORK_PROFILES_DIR="$STATE_DIR/networks"
DEFAULT_BANDWIDTH="200mbit"
MIN_KERNEL_VERSION="5.15"

# Tiered expiry system based on connection frequency
EXPIRY_DAILY=180      # 6 months for networks used 5-7x per week
EXPIRY_REGULAR=90     # 3 months for networks used 2-4x per week
EXPIRY_OCCASIONAL=30  # 1 month for networks used <2x per week
EXPIRY_NEW=90         # Default for new networks (3 months)

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
    local required_cmds=("ip" "nmcli" "iw" "tc" "ethtool" "sysctl")
    
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
    # pick first wireless interface in state UP or DOWN
    local ifc
    ifc=$(ip -o link show | awk -F': ' '{print $2}' | grep -E '^wl' | head -n1)
    
    # If no wireless, look for ethernet (en* or eth*) that is UP
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

function get_connection_frequency() {
    local profile_file="$1"
    [[ ! -f "$profile_file" ]] && echo "0" && return
    
    # Get connection count and created date
    local connection_count=$(grep '^CONNECTION_COUNT=' "$profile_file" 2>/dev/null | cut -d= -f2)
    local created_date=$(grep '^CREATED_DATE=' "$profile_file" 2>/dev/null | cut -d= -f2)
    
    connection_count=${connection_count:-1}
    created_date=${created_date:-$(date +%s)}
    
    local now=$(date +%s)
    local age_weeks=$(( (now - created_date) / 604800 ))
    
    # Avoid division by zero
    [[ $age_weeks -lt 1 ]] && age_weeks=1
    
    # Calculate connections per week
    local connections_per_week=$((connection_count / age_weeks))
    echo "$connections_per_week"
}

function calculate_expiry_days() {
    local profile_file="$1"
    
    if [[ ! -f "$profile_file" ]]; then
        echo "$EXPIRY_NEW"
        return
    fi
    
    local freq=$(get_connection_frequency "$profile_file")
    
    # Tiered expiry based on usage frequency
    if [[ $freq -ge 5 ]]; then
        echo "$EXPIRY_DAILY"        # Daily use: 6 months
    elif [[ $freq -ge 2 ]]; then
        echo "$EXPIRY_REGULAR"      # Regular use: 3 months
    else
        echo "$EXPIRY_OCCASIONAL"  # Occasional use: 1 month
    fi
}

function is_profile_expired() {
    local profile_file="$1"
    [[ ! -f "$profile_file" ]] && return 0  # Doesn't exist = expired
    
    # Check if profile has expiry date
    if ! grep -q '^EXPIRY_DATE=' "$profile_file"; then
        # Old profile without expiry - migrate it
        migrate_profile "$profile_file"
    fi
    
    source "$profile_file"
    local expiry_epoch="${EXPIRY_DATE:-0}"
    local now_epoch=$(date +%s)
    
    [[ $now_epoch -gt $expiry_epoch ]]
}

function migrate_profile() {
    local profile_file="$1"
    [[ ! -f "$profile_file" ]] && return
    
    # Add creation and expiry dates to old profiles
    if ! grep -q '^CREATED_DATE=' "$profile_file"; then
        local now_epoch=$(date +%s)
        local expiry_days=$EXPIRY_NEW
        local expiry_epoch=$((now_epoch + expiry_days * 86400))
        
        echo "CREATED_DATE=$now_epoch" >> "$profile_file"
        echo "EXPIRY_DATE=$expiry_epoch" >> "$profile_file"
        echo "CONNECTION_COUNT=1" >> "$profile_file"
        log_info "Migrated profile $(basename "$profile_file") with ${expiry_days}-day expiry"
    fi
    
    # Add connection count if missing
    if ! grep -q '^CONNECTION_COUNT=' "$profile_file"; then
        echo "CONNECTION_COUNT=1" >> "$profile_file"
    fi
}

function renew_profile() {
    local profile_file="$1"
    [[ ! -f "$profile_file" ]] && return 1
    
    # Increment connection count
    local count=$(grep '^CONNECTION_COUNT=' "$profile_file" 2>/dev/null | cut -d= -f2)
    count=${count:-0}
    count=$((count + 1))
    
    if grep -q '^CONNECTION_COUNT=' "$profile_file"; then
        sed -i "s/^CONNECTION_COUNT=.*/CONNECTION_COUNT=$count/" "$profile_file"
    else
        echo "CONNECTION_COUNT=$count" >> "$profile_file"
    fi
    
    # Calculate new expiry based on connection frequency
    local expiry_days=$(calculate_expiry_days "$profile_file")
    local now_epoch=$(date +%s)
    local expiry_epoch=$((now_epoch + expiry_days * 86400))
    
    # Update expiry date in file
    if grep -q '^EXPIRY_DATE=' "$profile_file"; then
        sed -i "s/^EXPIRY_DATE=.*/EXPIRY_DATE=$expiry_epoch/" "$profile_file"
    else
        echo "EXPIRY_DATE=$expiry_epoch" >> "$profile_file"
    fi
    
    # Log the tier if verbose
    local freq=$(get_connection_frequency "$profile_file")
    log_info "Renewed profile: ${count} connections, ${freq}/week frequency, ${expiry_days}-day expiry"
    
    return 0
}

function get_network_profile() {
    local ssid="$1"
    local safe_name
    safe_name=$(sanitize_ssid "$ssid")
    echo "$NETWORK_PROFILES_DIR/${safe_name}.conf"
}

function load_network_profile() {
    local ssid="$1"
    local profile
    profile=$(get_network_profile "$ssid")
    
    if [[ -f "$profile" ]]; then
        source "$profile"
        # Check if expired
        if is_profile_expired "$profile"; then
            log_info "Profile for $ssid has expired. Re-evaluating..."
            return 1
        fi
        # Renew profile on successful load (active use)
        renew_profile "$profile"
        return 0
    fi
    return 1
}

function save_network_profile() {
    local ssid="$1"
    local bandwidth="$2"
    local power_mode="${3:-auto}"  # auto, always-off, always-on
    local profile
    profile=$(get_network_profile "$ssid")
    
    local now_epoch=$(date +%s)
    local expiry_days=$EXPIRY_NEW
    local expiry_epoch=$((now_epoch + expiry_days * 86400))
    
    cat > "$profile" << EOF
# Network profile for: $ssid
# Generated: $(date)
# Expires: $(date -d "@$expiry_epoch" '+%Y-%m-%d %H:%M:%S' 2>/dev/null || date -r $expiry_epoch '+%Y-%m-%d %H:%M:%S' 2>/dev/null || echo "in ${expiry_days} days")
# Expiry tier: New network (will adjust based on usage frequency)
SSID="$ssid"
BANDWIDTH="$bandwidth"
POWER_MODE="$power_mode"
CREATED_DATE=$now_epoch
EXPIRY_DATE=$expiry_epoch
CONNECTION_COUNT=1
EOF
    log_success "Saved profile for network: $ssid (expires in $expiry_days days, adjusts with usage)"
}

function enable_iwd() {
    log_info "Checking for iwd..."
    if ! command -v iwd &>/dev/null; then
        log_warning "iwd not found. Installing..."
        install_dependencies "iwd" || {
            log_error "Could not install iwd. Skipping backend switch."
            return 1
        }
    fi

    log_info "Switching NetworkManager backend to iwd..."
    
    # Create configuration to switch backend
    mkdir -p /etc/NetworkManager/conf.d
    cat > /etc/NetworkManager/conf.d/wifi_backend.conf <<EOF
[device]
wifi.backend=iwd
EOF

    # Mask wpa_supplicant to prevent conflicts
    systemctl mask wpa_supplicant.service 2>/dev/null || true
    systemctl stop wpa_supplicant.service 2>/dev/null || true

    # Enable and start iwd
    systemctl enable --now iwd.service 2>/dev/null || true

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
