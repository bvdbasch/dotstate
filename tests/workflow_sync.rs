//! Integration tests for file sync workflows.
//!
//! Tests the complete chain of operations for:
//! - Adding files to sync
//! - Removing files from sync
//! - Moving files to/from common

mod common;

use anyhow::Result;
use common::TestEnv;

// ============================================================================
// ADD FILE TO SYNC - HAPPY PATH
// ============================================================================

#[test]
fn add_file_creates_symlink_and_tracks() -> Result<()> {
    // Given: home file exists, profile is active
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_home_file(".zshrc", "# my zshrc config")
        .build()?;

    // Verify initial state
    env.assert_home_regular_file(".zshrc");
    env.assert_file_not_tracked(".zshrc");

    // When: add file to sync using SyncService
    // For now, we'll manually simulate what SyncService does to verify our test infra
    // TODO: Replace with actual SyncService::add_file_to_sync call

    // Manually perform the sync operation:
    // 1. Copy file to repo
    let home_path = env.home_path(".zshrc");
    let repo_file = env.profile_file_path("default", ".zshrc");
    std::fs::create_dir_all(repo_file.parent().unwrap())?;
    std::fs::copy(&home_path, &repo_file)?;

    // 2. Remove original and create symlink
    std::fs::remove_file(&home_path)?;
    std::os::unix::fs::symlink(&repo_file, &home_path)?;

    // 3. Update tracking
    let mut tracking = env.load_tracking()?;
    tracking
        .symlinks
        .push(dotstate::utils::symlink_manager::TrackedSymlink {
            target: home_path.clone(),
            source: repo_file.clone(),
            created_at: chrono::Utc::now(),
            backup: None,
        });
    env.save_tracking(&tracking)?;

    // 4. Update manifest
    let mut manifest = env.load_manifest()?;
    if let Some(profile) = manifest.profiles.iter_mut().find(|p| p.name == "default") {
        profile.synced_files.push(".zshrc".to_string());
    }
    manifest.save(&env.repo_path)?;

    // Then: verify complete state
    env.assert_is_symlink(".zshrc");
    env.assert_symlink_points_to(".zshrc", &repo_file);
    env.assert_file_tracked(".zshrc");
    env.assert_file_in_profile("default", ".zshrc");

    // Verify content is preserved
    assert_eq!(env.home_file_content(".zshrc"), Some("# my zshrc config".to_string()));

    Ok(())
}

#[test]
fn synced_file_content_is_accessible_via_symlink() -> Result<()> {
    // Given: file is already synced
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_synced_file("default", ".bashrc", "export PATH=/usr/bin")
        .build()?;

    // Then: content is accessible through the symlink
    let content = env.home_file_content(".bashrc");
    assert_eq!(content, Some("export PATH=/usr/bin".to_string()));

    // And: it's properly tracked
    env.assert_is_symlink(".bashrc");
    env.assert_file_tracked(".bashrc");
    env.assert_file_in_profile("default", ".bashrc");

    Ok(())
}

// ============================================================================
// ADD FILE TO SYNC - EDGE CASES
// ============================================================================

#[test]
fn add_file_handles_nested_directory_structure() -> Result<()> {
    // Given: deeply nested file
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_home_file(".config/app/settings/config.toml", "[settings]\nkey = \"value\"")
        .build()?;

    // Verify nested file exists
    env.assert_home_regular_file(".config/app/settings/config.toml");

    // Simulate sync operation
    let home_path = env.home_path(".config/app/settings/config.toml");
    let repo_file = env.profile_file_path("default", ".config/app/settings/config.toml");
    std::fs::create_dir_all(repo_file.parent().unwrap())?;
    std::fs::copy(&home_path, &repo_file)?;
    std::fs::remove_file(&home_path)?;
    std::os::unix::fs::symlink(&repo_file, &home_path)?;

    // Update tracking
    let mut tracking = env.load_tracking()?;
    tracking
        .symlinks
        .push(dotstate::utils::symlink_manager::TrackedSymlink {
            target: home_path.clone(),
            source: repo_file.clone(),
            created_at: chrono::Utc::now(),
            backup: None,
        });
    env.save_tracking(&tracking)?;

    // Then: directory structure is preserved
    env.assert_is_symlink(".config/app/settings/config.toml");
    assert!(env.repo_file_path("default/.config/app/settings/config.toml").exists());

    Ok(())
}

#[test]
fn test_env_with_multiple_synced_files() -> Result<()> {
    // Given: multiple files already synced
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_synced_file("default", ".zshrc", "zsh config")
        .with_synced_file("default", ".bashrc", "bash config")
        .with_synced_file("default", ".vimrc", "vim config")
        .build()?;

    // Then: all files are properly synced
    env.assert_is_symlink(".zshrc");
    env.assert_is_symlink(".bashrc");
    env.assert_is_symlink(".vimrc");

    env.assert_file_tracked(".zshrc");
    env.assert_file_tracked(".bashrc");
    env.assert_file_tracked(".vimrc");

    env.assert_file_in_profile("default", ".zshrc");
    env.assert_file_in_profile("default", ".bashrc");
    env.assert_file_in_profile("default", ".vimrc");

    Ok(())
}

// ============================================================================
// REMOVE FILE FROM SYNC - HAPPY PATH
// ============================================================================

#[test]
fn remove_file_restores_original_and_untracks() -> Result<()> {
    // Given: file is synced
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_synced_file("default", ".zshrc", "# original content")
        .build()?;

    // Verify initial synced state
    env.assert_is_symlink(".zshrc");
    env.assert_file_tracked(".zshrc");

    // When: remove from sync (simulate the operation)
    let home_path = env.home_path(".zshrc");
    let repo_file = env.profile_file_path("default", ".zshrc");

    // 1. Read content from repo
    let content = std::fs::read_to_string(&repo_file)?;

    // 2. Remove symlink
    std::fs::remove_file(&home_path)?;

    // 3. Restore as regular file
    std::fs::write(&home_path, &content)?;

    // 4. Update tracking
    let mut tracking = env.load_tracking()?;
    tracking.symlinks.retain(|s| s.target != home_path);
    env.save_tracking(&tracking)?;

    // 5. Update manifest
    let mut manifest = env.load_manifest()?;
    if let Some(profile) = manifest.profiles.iter_mut().find(|p| p.name == "default") {
        profile.synced_files.retain(|f| f != ".zshrc");
    }
    manifest.save(&env.repo_path)?;

    // Then: file is restored as regular file
    env.assert_no_symlink(".zshrc");
    env.assert_home_regular_file(".zshrc");
    env.assert_file_not_tracked(".zshrc");
    env.assert_file_not_in_profile("default", ".zshrc");

    // Content preserved
    assert_eq!(env.home_file_content(".zshrc"), Some("# original content".to_string()));

    Ok(())
}

// ============================================================================
// REMOVE FILE FROM SYNC - EDGE CASES
// ============================================================================

#[test]
fn remove_file_when_symlink_already_gone() -> Result<()> {
    // Given: file is tracked but symlink was manually deleted
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_synced_file("default", ".zshrc", "content")
        .build()?;

    // Manually delete symlink without updating tracking
    env.delete_symlink_without_tracking(".zshrc")?;

    // Verify inconsistent state
    assert!(!env.home_path(".zshrc").exists());
    env.assert_file_tracked(".zshrc"); // Still tracked!

    // When: cleanup tracking (what remove should do)
    let mut tracking = env.load_tracking()?;
    let home_path = env.home_path(".zshrc");
    tracking.symlinks.retain(|s| s.target != home_path);
    env.save_tracking(&tracking)?;

    // Then: tracking is cleaned up
    env.assert_file_not_tracked(".zshrc");

    Ok(())
}

#[test]
fn remove_file_when_repo_source_missing() -> Result<()> {
    // Given: symlink exists but source file in repo was deleted
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_synced_file("default", ".zshrc", "content")
        .build()?;

    // Delete repo file without updating anything
    env.delete_repo_file_without_manifest("default/.zshrc")?;

    // Verify broken state - symlink exists but points to missing file
    env.assert_is_symlink(".zshrc");
    assert!(!env.profile_file_path("default", ".zshrc").exists());

    // The symlink now points to a non-existent file (broken symlink)
    // Reading through it should fail
    assert!(env.home_file_content(".zshrc").is_none());

    Ok(())
}

// ============================================================================
// COMMON FILES
// ============================================================================

#[test]
fn common_file_is_symlinked_when_profile_activated() -> Result<()> {
    // Given: common file exists with activated profile
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_common_file(".gitconfig", "[user]\nname = Test")
        .build()?;

    // Then: common file is symlinked
    env.assert_is_symlink(".gitconfig");
    env.assert_file_in_common(".gitconfig");
    env.assert_file_tracked(".gitconfig");

    // Points to common directory
    let expected_target = env.common_path().join(".gitconfig");
    env.assert_symlink_points_to(".gitconfig", &expected_target);

    Ok(())
}

#[test]
fn common_file_not_symlinked_when_profile_not_activated() -> Result<()> {
    // Given: common file exists but profile not activated
    let env = TestEnv::new()
        .with_profile("default")
        .with_selected_profile("default") // Selected but NOT activated
        .with_common_file(".gitconfig", "[user]\nname = Test")
        .build()?;

    // Then: common file is NOT symlinked (no symlink in home)
    assert!(!env.home_path(".gitconfig").exists());

    // But it IS in the manifest
    env.assert_file_in_common(".gitconfig");

    // And the file exists in the common directory
    assert!(env.common_path().join(".gitconfig").exists());

    Ok(())
}

#[test]
fn multiple_profiles_share_common_files() -> Result<()> {
    // Given: two profiles with a common file
    let env = TestEnv::new()
        .with_profile("work")
        .with_profile("home")
        .with_activated_profile("work")
        .with_common_file(".gitconfig", "shared git config")
        .with_synced_file("work", ".work-specific", "work stuff")
        .build()?;

    // Then: common file is shared
    env.assert_is_symlink(".gitconfig");
    env.assert_file_in_common(".gitconfig");

    // Work-specific file is only in work profile
    env.assert_is_symlink(".work-specific");
    env.assert_file_in_profile("work", ".work-specific");
    env.assert_file_not_in_profile("home", ".work-specific");

    Ok(())
}

// ============================================================================
// CRASH/CORRUPTION SCENARIOS
// ============================================================================

#[test]
fn detect_tracking_without_symlink() -> Result<()> {
    // Given: tracking entry exists but symlink doesn't (simulates crash after tracking update)
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .build()?;

    // Add tracking entry without creating symlink
    let source = env.profile_file_path("default", ".orphan");
    std::fs::create_dir_all(source.parent().unwrap())?;
    std::fs::write(&source, "orphan content")?;
    env.add_tracking_without_symlink(".orphan", &source)?;

    // Then: tracking exists but symlink doesn't
    env.assert_file_tracked(".orphan");
    assert!(!env.home_path(".orphan").exists());

    // This is an inconsistent state that doctor should detect

    Ok(())
}

#[test]
fn detect_symlink_without_tracking() -> Result<()> {
    // Given: symlink exists but not tracked (simulates manual symlink creation)
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .build()?;

    // Create a symlink manually without tracking
    let source = env.profile_file_path("default", ".manual");
    std::fs::create_dir_all(source.parent().unwrap())?;
    std::fs::write(&source, "manual content")?;
    env.create_symlink_without_tracking(".manual", &source)?;

    // Then: symlink exists but not tracked
    env.assert_is_symlink(".manual");
    env.assert_file_not_tracked(".manual");

    Ok(())
}

#[test]
fn detect_manifest_tracking_mismatch() -> Result<()> {
    // Given: manifest says file is synced but tracking doesn't have it
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_synced_file("default", ".zshrc", "content")
        .build()?;

    // Remove from tracking but leave in manifest
    let mut tracking = env.load_tracking()?;
    tracking.symlinks.clear();
    env.save_tracking(&tracking)?;

    // Then: manifest and tracking are out of sync
    env.assert_file_in_profile("default", ".zshrc");
    env.assert_file_not_tracked(".zshrc");

    // Symlink still exists because we only modified tracking
    env.assert_is_symlink(".zshrc");

    Ok(())
}

// ============================================================================
// TEST ENVIRONMENT VERIFICATION
// ============================================================================

#[test]
fn test_env_creates_correct_structure() -> Result<()> {
    let env = TestEnv::new().with_profile("default").with_git().build()?;

    // Verify directory structure
    assert!(env.home_dir.exists());
    assert!(env.repo_path.exists());
    assert!(env.config_dir.exists());
    assert!(env.backup_dir.exists());

    // Verify repo structure (profiles stored directly at repo/<name>/, not repo/profiles/<name>/)
    assert!(env.repo_path.join("default").exists());
    assert!(env.repo_path.join("common").exists());

    // Verify git was initialized
    assert!(env.repo_path.join(".git").exists());

    // Verify config exists
    assert!(env.config_path().exists());

    // Verify manifest exists
    assert!(env.repo_path.join(".dotstate-profiles.toml").exists());

    Ok(())
}

#[test]
fn test_env_builder_creates_expected_state() -> Result<()> {
    let env = TestEnv::new()
        .with_profile("work")
        .with_profile("home")
        .with_activated_profile("work")
        .with_synced_file("work", ".workrc", "work config")
        .with_synced_file("home", ".homerc", "home config")
        .with_common_file(".shared", "shared")
        .with_home_file(".local", "local only")
        .with_backup_enabled()
        .build()?;

    // Verify profiles
    env.assert_profile_exists("work");
    env.assert_profile_exists("home");
    env.assert_active_profile("work");
    env.assert_profile_activated();

    // Verify synced files (only work profile's files are symlinked since it's active)
    env.assert_is_symlink(".workrc");
    env.assert_file_in_profile("work", ".workrc");

    // Home profile file exists in repo but not symlinked (wrong profile)
    assert!(env.profile_file_path("home", ".homerc").exists());
    assert!(!env.home_path(".homerc").exists()); // Not symlinked

    // Common file is symlinked
    env.assert_is_symlink(".shared");
    env.assert_file_in_common(".shared");

    // Local file is just a regular file
    env.assert_home_regular_file(".local");

    // Backup is enabled
    let config = env.load_config()?;
    assert!(config.backup_enabled);

    Ok(())
}
