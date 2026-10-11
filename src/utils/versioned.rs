//! File versioning utilities for safe schema migrations.
//!
//! Provides helpers for migrating configuration and data files between versions.
//! Files without a version field are treated as v0.
//!
//! ## Usage
//!
//! Each versionable struct should:
//! 1. Have a `version: u32` field with `#[serde(default)]`
//! 2. Define a `CURRENT_VERSION` constant
//! 3. Implement a `migrate()` chain that upgrades through versions
//! 4. Use `migrate_file()` helper for backup/save/cleanup
//!
//! ## Example
//!
//! ```ignore
//! const CURRENT_VERSION: u32 = 1;
//!
//! impl Config {
//!     pub fn load(path: &Path) -> Result<Self> {
//!         let content = fs::read_to_string(path)?;
//!         let mut config: Config = toml::from_str(&content)?;
//!
//!         if config.version < CURRENT_VERSION {
//!             let old_version = config.version;
//!             config = Self::migrate(config)?;
//!             versioned::migrate_file(path, &config, old_version, "toml", |c, p| c.save(p))?;
//!         }
//!
//!         Ok(config)
//!     }
//! }
//! ```

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Perform file migration with backup safety.
///
/// Creates a backup before saving, then removes the backup on success.
/// If save fails, the backup remains for manual recovery.
///
/// # Arguments
/// * `path` - Path to the file being migrated
/// * `old_version` - Version before migration (used in backup filename)
/// * `extension` - File extension without dot (e.g., "toml", "json")
/// * `save_fn` - Function that saves the migrated content
///
/// # Backup naming
/// Backup files are named `<filename>.<ext>.backup-v<old_version>`
/// e.g., `config.toml.backup-v0`
pub fn migrate_file<F>(path: &Path, old_version: u32, extension: &str, save_fn: F) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    // Create backup
    let backup_path = path.with_extension(format!("{}.backup-v{}", extension, old_version));
    fs::copy(path, &backup_path)
        .with_context(|| format!("Failed to create backup at {} before migration", backup_path.display()))?;

    // Run save function
    save_fn().with_context(|| format!("Failed to save migrated file to {}", path.display()))?;

    // Success - remove backup
    let _ = fs::remove_file(&backup_path);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_migrate_file_creates_and_removes_backup() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("config.toml");

        // Create original file
        fs::write(&file_path, "version = 0\nname = \"test\"").unwrap();

        // Migrate
        let result = migrate_file(&file_path, 0, "toml", || {
            fs::write(&file_path, "version = 1\nname = \"test\"")?;
            Ok(())
        });

        assert!(result.is_ok());

        // Backup should be removed
        let backup_path = file_path.with_extension("toml.backup-v0");
        assert!(!backup_path.exists(), "Backup should be removed on success");

        // File should be updated
        let content = fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("version = 1"));
    }

    #[test]
    fn test_migrate_file_keeps_backup_on_failure() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("config.toml");

        // Create original file
        fs::write(&file_path, "version = 0\nname = \"test\"").unwrap();

        // Migrate with failure
        let result = migrate_file(&file_path, 0, "toml", || anyhow::bail!("Simulated migration failure"));

        assert!(result.is_err());

        // Backup should remain
        let backup_path = file_path.with_extension("toml.backup-v0");
        assert!(backup_path.exists(), "Backup should remain on failure");

        // Original content should be in backup
        let backup_content = fs::read_to_string(&backup_path).unwrap();
        assert!(backup_content.contains("version = 0"));
    }
}
