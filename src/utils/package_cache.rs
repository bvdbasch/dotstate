use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::{debug, info, warn};

/// Current version of the `package_status.json` file format.
/// Increment this when making breaking changes to the schema.
const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageCacheEntry {
    pub installed: bool,
    pub last_checked: DateTime<Utc>,
    pub check_command: Option<String>,
    pub output: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PackageCacheData {
    #[serde(default)]
    pub version: u32,
    // Key format: "profile_name::package_name"
    #[serde(default)]
    pub entries: HashMap<String, PackageCacheEntry>,
}

impl Default for PackageCacheData {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            entries: HashMap::new(),
        }
    }
}

impl PackageCacheData {
    // ==================== Migration Methods ====================

    /// Run all necessary migrations to bring cache data to current version.
    fn migrate(mut data: Self) -> Result<Self> {
        if data.version == 0 {
            data = Self::migrate_v0_to_v1(data)?;
        }
        // Future migrations:
        // if data.version == 1 { data = Self::migrate_v1_to_v2(data)?; }
        Ok(data)
    }

    /// Migrate from v0 (no version field) to v1.
    /// This is a no-op migration that just sets the version field.
    fn migrate_v0_to_v1(mut data: Self) -> Result<Self> {
        debug!("Migrating package_status.json v0 -> v1");
        data.version = 1;
        Ok(data)
    }
}

#[derive(Debug)]
pub struct PackageCache {
    cache_file: PathBuf,
    data: PackageCacheData,
}

impl Default for PackageCache {
    fn default() -> Self {
        // Try to load from default location, otherwise fall back to empty memory-only cache (which won't save correctly if path is bad, but satisfies trait)
        match Self::new() {
            Ok(cache) => cache,
            Err(e) => {
                warn!("Failed to initialize package cache with default path: {}", e);
                // Fallback to a dummy path that probably won't write successfully but allows the app to validly construct the struct.
                // Or better: use a sensible default path even if we couldn't create it right now.
                let config_dir = crate::utils::get_config_dir();
                Self {
                    cache_file: config_dir.join("package_status.json"),
                    data: PackageCacheData::default(),
                }
            }
        }
    }
}

impl PackageCache {
    pub fn new() -> Result<Self> {
        let config_dir = crate::utils::get_config_dir();
        let cache_file = config_dir.join("package_status.json");

        let mut data: PackageCacheData = if cache_file.exists() {
            match std::fs::read_to_string(&cache_file) {
                Ok(content) => match serde_json::from_str(&content) {
                    Ok(data) => data,
                    Err(e) => {
                        warn!("Failed to parse package cache: {}", e);
                        PackageCacheData::default()
                    }
                },
                Err(e) => {
                    warn!("Failed to read package cache: {}", e);
                    PackageCacheData::default()
                }
            }
        } else {
            PackageCacheData::default()
        };

        // Migrate if needed
        if cache_file.exists() && data.version < CURRENT_VERSION {
            let old_version = data.version;
            info!(
                "Migrating package_status.json from v{} to v{}",
                old_version, CURRENT_VERSION
            );
            data = PackageCacheData::migrate(data)?;

            // Backup, save, cleanup
            let cache_json = serde_json::to_string_pretty(&data).context("Failed to serialize package cache")?;
            super::migrate_file(&cache_file, old_version, "json", || {
                std::fs::write(&cache_file, &cache_json).context("Failed to write package cache")
            })?;
        }

        Ok(Self { cache_file, data })
    }

    fn get_key(profile_name: &str, package_name: &str) -> String {
        format!("{profile_name}::{package_name}")
    }

    #[must_use]
    pub fn get_status(&self, profile_name: &str, package_name: &str) -> Option<&PackageCacheEntry> {
        self.data.entries.get(&Self::get_key(profile_name, package_name))
    }

    pub fn update_status(
        &mut self,
        profile_name: &str,
        package_name: &str,
        installed: bool,
        check_command: Option<String>,
        output: Option<String>,
    ) -> Result<()> {
        let key = Self::get_key(profile_name, package_name);

        let entry = PackageCacheEntry {
            installed,
            last_checked: Utc::now(),
            check_command,
            output,
        };

        self.data.entries.insert(key, entry);
        self.save()
    }

    pub fn remove_status(&mut self, profile_name: &str, package_name: &str) -> Result<()> {
        let key = Self::get_key(profile_name, package_name);
        if self.data.entries.remove(&key).is_some() {
            debug!("Removed cache entry for {}", key);
            self.save()?;
        }
        Ok(())
    }

    /// Save package cache to file.
    /// Uses atomic write (temp file + rename) to prevent corruption on crash.
    fn save(&self) -> Result<()> {
        let temp_path = self.cache_file.with_extension("json.tmp");

        if let Some(parent) = self.cache_file.parent() {
            std::fs::create_dir_all(parent).context("Failed to create config directory")?;
        }

        let json = serde_json::to_string_pretty(&self.data).context("Failed to serialize package cache")?;

        // Write to temp file first
        std::fs::write(&temp_path, &json).context("Failed to write temp package cache")?;

        // Atomic rename (on POSIX systems)
        std::fs::rename(&temp_path, &self.cache_file).context("Failed to rename temp package cache")?;

        debug!("Package cache saved to {:?}", self.cache_file);
        Ok(())
    }
}
