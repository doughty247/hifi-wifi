//! Firmware download from linux-firmware.git
//!
//! Downloads firmware files from GitLab and validates them before deployment.

use anyhow::{Result, Context, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::firmware::version::FirmwareVersion;

/// Base URL for linux-firmware.git raw files
const FIRMWARE_BASE_URL: &str = "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/main/ath11k/QCA2066/hw2.1";

/// Firmware files to download
const FIRMWARE_FILES: &[FirmwareFile] = &[
    FirmwareFile {
        name: "amss.bin",
        min_size: 5_000_000,  // ~5.3MB actual
        description: "Main WiFi firmware",
    },
    FirmwareFile {
        name: "m3.bin",
        min_size: 200_000,    // ~260KB actual
        description: "M3 microcontroller firmware",
    },
    FirmwareFile {
        name: "board-2.bin",
        min_size: 500_000,    // ~745KB actual
        description: "Board configuration data",
    },
];

/// Firmware file metadata
struct FirmwareFile {
    name: &'static str,
    min_size: u64,
    #[allow(dead_code)]
    description: &'static str,
}

/// Firmware downloader using system curl
pub struct FirmwareDownloader;

impl FirmwareDownloader {
    /// Create a new downloader
    pub fn new() -> Result<Self> {
        // Verify curl is available
        let output = Command::new("curl")
            .arg("--version")
            .output()
            .context("curl command not found - please install curl")?;

        if !output.status.success() {
            bail!("curl command failed");
        }

        Ok(Self)
    }

    /// Download all firmware files to a staging directory
    ///
    /// Returns the path to the staging directory on success
    pub fn download_all(&self) -> Result<PathBuf> {
        // Create staging directory
        let staging_dir = tempfile::Builder::new()
            .prefix("hifi-firmware-")
            .tempdir()
            .context("Failed to create staging directory")?
            .into_path();

        for file in FIRMWARE_FILES {
            self.download_file(file, &staging_dir)?;
        }

        Ok(staging_dir)
    }

    /// Download a single firmware file using curl
    fn download_file(&self, file: &FirmwareFile, staging_dir: &Path) -> Result<()> {
        let url = format!("{}/{}", FIRMWARE_BASE_URL, file.name);
        let dest_path = staging_dir.join(file.name);

        print!("  Downloading {}... ", file.name);
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let output = Command::new("curl")
            .args([
                "-sfL",                              // silent, fail on error, follow redirects
                "--max-time", "120",                 // 2 min timeout for large files
                "-o",
            ])
            .arg(&dest_path)
            .arg(&url)
            .output()
            .with_context(|| format!("Failed to run curl to download {}", file.name))?;

        if !output.status.success() {
            println!("FAILED");
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to download {}: {}", file.name, stderr);
        }

        // Validate size
        let metadata = fs::metadata(&dest_path)
            .with_context(|| format!("Failed to read downloaded {}", file.name))?;

        let size = metadata.len();
        if size < file.min_size {
            println!("FAILED");
            bail!(
                "Downloaded {} is too small ({} bytes, expected >= {}). File may be corrupted.",
                file.name, size, file.min_size
            );
        }

        let size_mb = size as f64 / 1_000_000.0;
        println!("{:.1} MB", size_mb);

        Ok(())
    }

    /// Validate downloaded firmware files
    ///
    /// Checks file sizes and verifies we can extract version from amss.bin
    pub fn validate(&self, staging_dir: &Path) -> Result<()> {
        // Verify all files exist and have reasonable sizes
        for file in FIRMWARE_FILES {
            let path = staging_dir.join(file.name);
            let metadata = fs::metadata(&path)
                .with_context(|| format!("Missing file: {}", file.name))?;

            if metadata.len() < file.min_size {
                bail!(
                    "{} is too small ({} bytes, expected >= {})",
                    file.name, metadata.len(), file.min_size
                );
            }

            print!("  Validating {}... ", file.name);
            println!("OK ({} bytes)", metadata.len());
        }

        // Verify we can extract version from amss.bin (proves it's valid firmware)
        print!("  Extracting version... ");
        let amss_path = staging_dir.join("amss.bin");
        let version = FirmwareVersion::from_raw(&amss_path)
            .context("Failed to extract version from downloaded firmware")?;

        if !version.version_string.contains("WLAN") {
            bail!("Downloaded firmware has unexpected version format: {}", version.version_string);
        }

        println!("{}", version.version_string);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]  // Requires network access
    fn test_download() {
        let downloader = FirmwareDownloader::new().unwrap();
        let staging = downloader.download_all().unwrap();
        downloader.validate(&staging).unwrap();

        // Cleanup
        fs::remove_dir_all(&staging).ok();
    }
}
