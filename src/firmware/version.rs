//! Firmware version detection and comparison
//!
//! Handles extracting version strings from firmware binaries and
//! fetching the latest upstream version from linux-firmware.git

use anyhow::{Result, Context, bail};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Firmware version information
#[derive(Debug, Clone)]
pub struct FirmwareVersion {
    /// Full version string (e.g., "WLAN.HSP.1.1-03926.13-QCAHSPSWPL_V2_SILICONZ_CE-2.52297.9")
    pub version_string: String,
}

impl FirmwareVersion {
    /// Extract version from an installed firmware file
    ///
    /// Decompresses amss.bin.zst and searches for the QC_IMAGE_VERSION_STRING
    pub fn from_installed(firmware_path: &Path) -> Result<Self> {
        let amss_path = firmware_path.join("amss.bin.zst");

        if !amss_path.exists() {
            bail!("Firmware file not found: {}", amss_path.display());
        }

        // Decompress and extract version
        let version_string = extract_version_from_zst(&amss_path)?;

        Ok(Self { version_string })
    }

    /// Extract version from an uncompressed firmware file
    pub fn from_raw(amss_path: &Path) -> Result<Self> {
        let version_string = extract_version_from_raw(amss_path)?;
        Ok(Self { version_string })
    }

    /// Check if this is Valve stock firmware (has CI_WLAN prefix)
    pub fn is_valve_stock(&self) -> bool {
        self.version_string.starts_with("CI_WLAN")
    }
}

/// Detect the firmware path for the QCA2066/ath11k device
///
/// Checks multiple possible paths (SteamOS uses QCA206X, upstream uses QCA2066)
pub fn detect_firmware_path() -> Result<PathBuf> {
    // Valve/SteamOS path (most likely on Steam Deck)
    let valve_path = Path::new("/lib/firmware/ath11k/QCA206X/hw2.1");
    if valve_path.exists() && valve_path.join("amss.bin.zst").exists() {
        return Ok(valve_path.to_path_buf());
    }

    // Upstream linux-firmware path
    let upstream_path = Path::new("/lib/firmware/ath11k/QCA2066/hw2.1");
    if upstream_path.exists() && upstream_path.join("amss.bin.zst").exists() {
        return Ok(upstream_path.to_path_buf());
    }

    // Generic WCN6855 path (same chip, different name)
    let generic_path = Path::new("/lib/firmware/ath11k/WCN6855/hw2.1");
    if generic_path.exists() && generic_path.join("amss.bin.zst").exists() {
        return Ok(generic_path.to_path_buf());
    }

    // Also check hw2.0 variants
    let generic_path_20 = Path::new("/lib/firmware/ath11k/WCN6855/hw2.0");
    if generic_path_20.exists() && generic_path_20.join("amss.bin.zst").exists() {
        return Ok(generic_path_20.to_path_buf());
    }

    bail!(
        "Firmware directory not found. Checked:\n\
         - /lib/firmware/ath11k/QCA206X/hw2.1/ (SteamOS)\n\
         - /lib/firmware/ath11k/QCA2066/hw2.1/ (upstream)\n\
         - /lib/firmware/ath11k/WCN6855/hw2.1/ (generic)"
    );
}

/// Extract version string from a zstd-compressed firmware file
fn extract_version_from_zst(zst_path: &Path) -> Result<String> {
    // Use system zstd command to decompress (avoids C compilation issues)
    let output = Command::new("zstd")
        .args(["-d", "-c"])
        .arg(zst_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("Failed to run zstd to decompress {}", zst_path.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("zstd decompression failed for {}: {}", zst_path.display(), stderr);
    }

    extract_version_from_bytes(&output.stdout)
}

/// Extract version string from an uncompressed firmware file
fn extract_version_from_raw(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?;

    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    extract_version_from_bytes(&data)
}

/// Extract QC_IMAGE_VERSION_STRING from bytes
///
/// Searches through the binary for the version string pattern
fn extract_version_from_bytes(data: &[u8]) -> Result<String> {
    // The version string is embedded in the binary as: QC_IMAGE_VERSION_STRING=<version>
    let pattern = b"QC_IMAGE_VERSION_STRING=";

    // Search for the pattern
    if let Some(pos) = find_subsequence(data, pattern) {
        let start = pos + pattern.len();

        // Find the end of the version string (null terminator or non-printable)
        let mut end = start;
        while end < data.len() && data[end] >= 0x20 && data[end] < 0x7F {
            end += 1;
        }

        if end > start {
            let version = String::from_utf8_lossy(&data[start..end]).to_string();
            return Ok(version);
        }
    }

    bail!("Could not find QC_IMAGE_VERSION_STRING in firmware binary")
}

/// Find a subsequence in a byte slice
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len())
        .position(|window| window == needle)
}

/// Fetch the latest upstream version from linux-firmware.git
///
/// Downloads the amss.bin file header to extract the version string
pub fn get_upstream_version() -> Result<FirmwareVersion> {
    // We could parse the WHENCE file, but it doesn't contain version strings.
    // Instead, we'll fetch just enough of amss.bin to extract the version.
    // The version string is typically within the first 1MB of the file.

    let url = "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/main/ath11k/QCA2066/hw2.1/amss.bin";

    // Use system curl to fetch partial file (first 1MB should contain version)
    let output = Command::new("curl")
        .args([
            "-sfL",                         // silent, fail on error, follow redirects
            "--range", "0-1048575",         // First 1MB
            "--max-time", "30",             // 30 second timeout
            url,
        ])
        .output()
        .context("Failed to run curl to fetch upstream firmware")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to fetch upstream firmware: {}", stderr);
    }

    let data = &output.stdout;

    // Search for version string
    let pattern = b"QC_IMAGE_VERSION_STRING=";
    if let Some(pos) = find_subsequence(data, pattern) {
        let start = pos + pattern.len();
        let mut end = start;
        while end < data.len() && data[end] >= 0x20 && data[end] < 0x7F {
            end += 1;
        }

        if end > start {
            let version = String::from_utf8_lossy(&data[start..end]).to_string();
            return Ok(FirmwareVersion {
                version_string: version,
            });
        }
    }

    bail!("Could not extract version from upstream firmware")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valve_stock_detection() {
        let valve = FirmwareVersion {
            version_string: "CI_WLAN.HSP.1.1-03926.9.1-QCAHSPSWPL_V2_SILICONZ_CE-15".to_string(),
        };
        assert!(valve.is_valve_stock());

        let upstream = FirmwareVersion {
            version_string: "WLAN.HSP.1.1-03926.13-QCAHSPSWPL_V2_SILICONZ_CE-2.52297.9".to_string(),
        };
        assert!(!upstream.is_valve_stock());
    }
}
