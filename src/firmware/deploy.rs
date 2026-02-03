//! Firmware deployment and backup management
//!
//! Handles:
//! - Creating backups before first update
//! - Compressing and deploying new firmware
//! - Reverting to backup
//! - SteamOS readonly filesystem handling

use anyhow::{Result, Context, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Sha256, Digest};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::firmware::version::FirmwareVersion;

/// Files we manage (NOT Data.msc - that's Valve-specific)
const MANAGED_FILES: &[&str] = &["amss.bin.zst", "m3.bin.zst", "board-2.bin.zst"];

/// Backup file suffix
const BACKUP_SUFFIX: &str = ".hifi-backup";

/// Backup metadata filename
const BACKUP_METADATA_FILE: &str = ".hifi-backup.json";

/// Zstd compression level (match SteamOS default)
const ZSTD_COMPRESSION_LEVEL: i32 = 19;

/// Backup metadata stored alongside backup files
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupInfo {
    /// Date backup was created
    pub backup_date: DateTime<Utc>,
    /// Whether backup is Valve stock firmware
    pub is_valve_stock: bool,
    /// Firmware version string
    pub version: String,
    /// SHA256 hashes of backup files
    pub files: std::collections::HashMap<String, FileHash>,
}

/// File hash information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileHash {
    pub sha256: String,
    pub size: u64,
}

/// Backup manager
pub struct BackupManager {
    firmware_path: PathBuf,
}

impl BackupManager {
    /// Create a new backup manager
    pub fn new(firmware_path: &Path) -> Self {
        Self {
            firmware_path: firmware_path.to_path_buf(),
        }
    }

    /// Check if backup files exist
    pub fn backup_files_exist(&self) -> bool {
        MANAGED_FILES.iter().all(|f| {
            self.firmware_path.join(format!("{}{}", f, BACKUP_SUFFIX)).exists()
        })
    }

    /// Get backup metadata (if exists)
    pub fn get_backup_info(&self) -> Option<BackupInfo> {
        let metadata_path = self.firmware_path.join(BACKUP_METADATA_FILE);
        if !metadata_path.exists() {
            return None;
        }

        let content = fs::read_to_string(&metadata_path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Create backup of current firmware
    pub fn create_backup(&self, current_version: &FirmwareVersion) -> Result<()> {
        let mut files = std::collections::HashMap::new();

        // Copy each managed file to backup
        for filename in MANAGED_FILES {
            let src = self.firmware_path.join(filename);
            let dst = self.firmware_path.join(format!("{}{}", filename, BACKUP_SUFFIX));

            if !src.exists() {
                bail!("Cannot backup: {} not found", src.display());
            }

            // Calculate hash before copying
            let hash = calculate_file_hash(&src)?;
            let metadata = fs::metadata(&src)?;

            fs::copy(&src, &dst)
                .with_context(|| format!("Failed to copy {} to backup", filename))?;

            files.insert(filename.to_string(), FileHash {
                sha256: hash,
                size: metadata.len(),
            });
        }

        // Write metadata
        let info = BackupInfo {
            backup_date: Utc::now(),
            is_valve_stock: current_version.is_valve_stock(),
            version: current_version.version_string.clone(),
            files,
        };

        let metadata_path = self.firmware_path.join(BACKUP_METADATA_FILE);
        let content = serde_json::to_string_pretty(&info)?;
        fs::write(&metadata_path, content)
            .context("Failed to write backup metadata")?;

        Ok(())
    }

    /// Verify backup integrity against stored hashes
    pub fn verify_integrity(&self, info: &BackupInfo) -> Result<()> {
        for (filename, expected) in &info.files {
            let backup_path = self.firmware_path.join(format!("{}{}", filename, BACKUP_SUFFIX));

            if !backup_path.exists() {
                bail!("Backup file missing: {}", backup_path.display());
            }

            let actual_hash = calculate_file_hash(&backup_path)?;
            if actual_hash != expected.sha256 {
                bail!(
                    "Backup file corrupted: {}\n  Expected: {}\n  Actual:   {}",
                    filename, expected.sha256, actual_hash
                );
            }

            let metadata = fs::metadata(&backup_path)?;
            if metadata.len() != expected.size {
                bail!(
                    "Backup file size mismatch: {} ({} != {})",
                    filename, metadata.len(), expected.size
                );
            }
        }

        Ok(())
    }

    /// Extract version from backup (when metadata is missing)
    pub fn extract_backup_version(&self) -> Result<String> {
        let backup_amss = self.firmware_path.join(format!("amss.bin.zst{}", BACKUP_SUFFIX));

        if !backup_amss.exists() {
            bail!("Backup amss.bin.zst not found");
        }

        // Decompress using system zstd command
        let output = Command::new("zstd")
            .args(["-d", "-c"])
            .arg(&backup_amss)
            .output()
            .context("Failed to run zstd to decompress backup")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("zstd decompression failed: {}", stderr);
        }

        let data = &output.stdout;
        let pattern = b"QC_IMAGE_VERSION_STRING=";
        if let Some(pos) = data.windows(pattern.len()).position(|w| w == pattern) {
            let start = pos + pattern.len();
            let mut end = start;
            while end < data.len() && data[end] >= 0x20 && data[end] < 0x7F {
                end += 1;
            }
            if end > start {
                return Ok(String::from_utf8_lossy(&data[start..end]).to_string());
            }
        }

        bail!("Could not extract version from backup")
    }
}

/// Firmware deployer
pub struct FirmwareDeployer {
    firmware_path: PathBuf,
}

impl FirmwareDeployer {
    /// Create a new deployer
    pub fn new(firmware_path: &Path) -> Self {
        Self {
            firmware_path: firmware_path.to_path_buf(),
        }
    }

    /// Deploy firmware from staging directory
    ///
    /// Compresses files and copies them atomically
    pub fn deploy(&self, staging_dir: &Path) -> Result<()> {
        // Handle SteamOS readonly filesystem
        let is_steamos = is_steamos();
        if is_steamos {
            disable_readonly()?;
        }

        let result = self.deploy_inner(staging_dir);

        // Re-enable readonly even if deploy failed
        if is_steamos {
            if let Err(e) = enable_readonly() {
                eprintln!("Warning: Failed to re-enable readonly: {}", e);
            }
        }

        result
    }

    /// Inner deploy logic (separated for readonly handling)
    fn deploy_inner(&self, staging_dir: &Path) -> Result<()> {
        // Map of source filename (without .zst) to compressed destination
        let files = [
            ("amss.bin", "amss.bin.zst"),
            ("m3.bin", "m3.bin.zst"),
            ("board-2.bin", "board-2.bin.zst"),
        ];

        // Phase 1: Compress all files to staging with .zst extension
        for (src_name, _dst_name) in &files {
            let src = staging_dir.join(src_name);
            let compressed = staging_dir.join(format!("{}.zst", src_name));

            compress_file(&src, &compressed)?;
        }

        // Phase 2: Copy to firmware directory with .new suffix
        for (_src_name, dst_name) in &files {
            let src = staging_dir.join(format!("{}", dst_name));
            let dst_new = self.firmware_path.join(format!("{}.new", dst_name));

            fs::copy(&src, &dst_new)
                .with_context(|| format!("Failed to copy {} to firmware directory", dst_name))?;
        }

        // Phase 3: Atomic rename .new to actual
        for (_src_name, dst_name) in &files {
            let dst_new = self.firmware_path.join(format!("{}.new", dst_name));
            let dst_final = self.firmware_path.join(dst_name);

            fs::rename(&dst_new, &dst_final)
                .with_context(|| format!("Failed to rename {} to final location", dst_name))?;
        }

        Ok(())
    }

    /// Restore firmware from backup
    pub fn restore_backup(&self) -> Result<()> {
        // Handle SteamOS readonly filesystem
        let is_steamos = is_steamos();
        if is_steamos {
            disable_readonly()?;
        }

        let result = self.restore_inner();

        if is_steamos {
            if let Err(e) = enable_readonly() {
                eprintln!("Warning: Failed to re-enable readonly: {}", e);
            }
        }

        result
    }

    /// Inner restore logic
    fn restore_inner(&self) -> Result<()> {
        // Phase 1: Copy backups to .new
        for filename in MANAGED_FILES {
            let backup = self.firmware_path.join(format!("{}{}", filename, BACKUP_SUFFIX));
            let dst_new = self.firmware_path.join(format!("{}.new", filename));

            if !backup.exists() {
                bail!("Backup file not found: {}", backup.display());
            }

            fs::copy(&backup, &dst_new)
                .with_context(|| format!("Failed to copy backup {}", filename))?;
        }

        // Phase 2: Atomic rename
        for filename in MANAGED_FILES {
            let dst_new = self.firmware_path.join(format!("{}.new", filename));
            let dst_final = self.firmware_path.join(filename);

            fs::rename(&dst_new, &dst_final)
                .with_context(|| format!("Failed to restore {}", filename))?;
        }

        Ok(())
    }
}

/// Calculate SHA256 hash of a file
fn calculate_file_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// Compress a file with zstd using system command
fn compress_file(src: &Path, dst: &Path) -> Result<()> {
    let output = Command::new("zstd")
        .arg(format!("-{}", ZSTD_COMPRESSION_LEVEL))
        .arg("-f")  // force overwrite
        .arg("-o")
        .arg(dst)
        .arg(src)
        .output()
        .with_context(|| format!("Failed to run zstd to compress {}", src.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("zstd compression failed for {}: {}", src.display(), stderr);
    }

    Ok(())
}

/// Check if running on SteamOS
fn is_steamos() -> bool {
    if let Ok(content) = fs::read_to_string("/etc/os-release") {
        content.contains("ID=steamos")
    } else {
        false
    }
}

/// Disable SteamOS readonly filesystem
fn disable_readonly() -> Result<()> {
    // Check if steamos-readonly command exists
    if !Path::new("/usr/bin/steamos-readonly").exists() {
        return Ok(());  // Not SteamOS or command not available
    }

    let output = Command::new("steamos-readonly")
        .arg("disable")
        .output()
        .context("Failed to run steamos-readonly disable")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "already disabled" type errors
        if !stderr.contains("already") && !output.status.success() {
            bail!("Failed to disable readonly mode: {}", stderr);
        }
    }

    Ok(())
}

/// Enable SteamOS readonly filesystem
fn enable_readonly() -> Result<()> {
    if !Path::new("/usr/bin/steamos-readonly").exists() {
        return Ok(());
    }

    let output = Command::new("steamos-readonly")
        .arg("enable")
        .output()
        .context("Failed to run steamos-readonly enable")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to enable readonly mode: {}", stderr);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_steamos() {
        // This will return false on non-SteamOS systems
        let result = is_steamos();
        println!("Is SteamOS: {}", result);
    }
}
