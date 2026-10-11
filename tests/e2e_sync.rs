//! End-to-end tests for sync operations using actual services.
//!
//! These tests use environment variable overrides to redirect DotState's
//! path functions to test directories, allowing us to test the real
//! SyncService, ProfileService, etc.
//!
//! **Run these tests serially to avoid env var conflicts:**
//! ```
//! cargo test --test e2e_sync -- --test-threads=1
//! ```

mod common;

use anyhow::Result;
use common::TestEnv;
use dotstate::services::SyncService;

// ============================================================================
// ADD FILE TO SYNC - USING REAL SERVICE
// ============================================================================

#[test]
fn e2e_add_file_creates_symlink_and_tracks() -> Result<()> {
    // Given: home file exists, profile is active, env overrides enabled
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_home_file(".zshrc", "# my zshrc config")
        .with_env_override()
        .build()?;

    // Verify initial state
    env.assert_home_regular_file(".zshrc");
    env.assert_file_not_tracked(".zshrc");

    // Load config (from our test config dir)
    let config = env.load_config()?;

    // When: add file to sync using REAL SyncService
    let full_path = env.home_path(".zshrc");
    let result = SyncService::add_file_to_sync(&config, &full_path, ".zshrc", false)?;

    // Then: operation succeeded
    assert!(
        matches!(result, dotstate::services::sync_service::AddFileResult::Success),
        "Expected Success, got {:?}",
        result
    );

    // Verify complete state
    env.assert_is_symlink(".zshrc");
    env.assert_file_tracked(".zshrc");
    env.assert_file_in_profile("default", ".zshrc");

    // Verify content is preserved (accessible through symlink)
    assert_eq!(env.home_file_content(".zshrc"), Some("# my zshrc config".to_string()));

    // Verify file exists in repo
    let repo_file = env.profile_file_path("default", ".zshrc");
    assert!(repo_file.exists(), "File should exist in repo");

    Ok(())
}

#[test]
fn e2e_add_file_with_nested_path() -> Result<()> {
    // Given: deeply nested file
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_home_file(".config/app/settings.toml", "[settings]\nvalue = 42")
        .with_env_override()
        .build()?;

    let config = env.load_config()?;

    // When: add nested file to sync
    let full_path = env.home_path(".config/app/settings.toml");
    let result = SyncService::add_file_to_sync(&config, &full_path, ".config/app/settings.toml", false)?;

    // Then: success with nested structure preserved
    assert!(matches!(
        result,
        dotstate::services::sync_service::AddFileResult::Success
    ));

    env.assert_is_symlink(".config/app/settings.toml");
    env.assert_file_tracked(".config/app/settings.toml");

    // Directory structure preserved in repo
    let repo_file = env.profile_file_path("default", ".config/app/settings.toml");
    assert!(repo_file.exists());

    Ok(())
}

#[test]
fn e2e_add_file_already_synced_returns_already_synced() -> Result<()> {
    // Given: file already synced
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_synced_file("default", ".bashrc", "bash config")
        .with_env_override()
        .build()?;

    let config = env.load_config()?;

    // When: try to add same file again
    let full_path = env.home_path(".bashrc");
    let result = SyncService::add_file_to_sync(&config, &full_path, ".bashrc", false)?;

    // Then: returns AlreadySynced
    assert!(
        matches!(result, dotstate::services::sync_service::AddFileResult::AlreadySynced),
        "Expected AlreadySynced, got {:?}",
        result
    );

    Ok(())
}

#[test]
fn e2e_add_nonexistent_file_returns_validation_failed() -> Result<()> {
    // Given: file doesn't exist
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_env_override()
        .build()?;

    let config = env.load_config()?;

    // When: try to add non-existent file
    let full_path = env.home_path(".nonexistent");
    let result = SyncService::add_file_to_sync(&config, &full_path, ".nonexistent", false)?;

    // Then: returns ValidationFailed
    assert!(
        matches!(
            result,
            dotstate::services::sync_service::AddFileResult::ValidationFailed(_)
        ),
        "Expected ValidationFailed, got {:?}",
        result
    );

    Ok(())
}

// ============================================================================
// REMOVE FILE FROM SYNC - USING REAL SERVICE
// ============================================================================

#[test]
fn e2e_remove_file_restores_and_untracks() -> Result<()> {
    // Given: file is synced
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_synced_file("default", ".zshrc", "original content")
        .with_env_override()
        .build()?;

    // Verify synced state
    env.assert_is_symlink(".zshrc");
    env.assert_file_tracked(".zshrc");

    let config = env.load_config()?;

    // When: remove file from sync using REAL service
    let result = SyncService::remove_file_from_sync(&config, ".zshrc")?;

    // Then: success
    assert!(
        matches!(result, dotstate::services::sync_service::RemoveFileResult::Success),
        "Expected Success, got {:?}",
        result
    );

    // File is now a regular file (not symlink)
    env.assert_no_symlink(".zshrc");
    env.assert_home_regular_file(".zshrc");

    // Content preserved
    assert_eq!(env.home_file_content(".zshrc"), Some("original content".to_string()));

    // No longer tracked
    env.assert_file_not_tracked(".zshrc");
    env.assert_file_not_in_profile("default", ".zshrc");

    Ok(())
}

#[test]
fn e2e_remove_not_synced_file_returns_not_synced() -> Result<()> {
    // Given: file is not synced
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_env_override()
        .build()?;

    let config = env.load_config()?;

    // When: try to remove file that's not synced
    let result = SyncService::remove_file_from_sync(&config, ".nonexistent")?;

    // Then: returns NotSynced
    assert!(
        matches!(result, dotstate::services::sync_service::RemoveFileResult::NotSynced),
        "Expected NotSynced, got {:?}",
        result
    );

    Ok(())
}

// ============================================================================
// MULTIPLE OPERATIONS - INTEGRATION
// ============================================================================

#[test]
fn e2e_add_then_remove_then_add_again() -> Result<()> {
    // Test the full lifecycle
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_home_file(".testrc", "test content v1")
        .with_env_override()
        .build()?;

    let config = env.load_config()?;
    let full_path = env.home_path(".testrc");

    // Step 1: Add file
    let result = SyncService::add_file_to_sync(&config, &full_path, ".testrc", false)?;
    assert!(matches!(
        result,
        dotstate::services::sync_service::AddFileResult::Success
    ));
    env.assert_is_symlink(".testrc");
    env.assert_file_tracked(".testrc");

    // Step 2: Remove file
    let result = SyncService::remove_file_from_sync(&config, ".testrc")?;
    assert!(matches!(
        result,
        dotstate::services::sync_service::RemoveFileResult::Success
    ));
    env.assert_no_symlink(".testrc");
    env.assert_file_not_tracked(".testrc");

    // Step 3: Add again
    let result = SyncService::add_file_to_sync(&config, &full_path, ".testrc", false)?;
    assert!(matches!(
        result,
        dotstate::services::sync_service::AddFileResult::Success
    ));
    env.assert_is_symlink(".testrc");
    env.assert_file_tracked(".testrc");

    Ok(())
}

#[test]
fn e2e_add_multiple_files() -> Result<()> {
    // Add several files and verify all are tracked
    let env = TestEnv::new()
        .with_profile("default")
        .with_activated_profile("default")
        .with_home_file(".file1", "content 1")
        .with_home_file(".file2", "content 2")
        .with_home_file(".file3", "content 3")
        .with_env_override()
        .build()?;

    let config = env.load_config()?;

    // Add all files
    for name in &[".file1", ".file2", ".file3"] {
        let full_path = env.home_path(name);
        let result = SyncService::add_file_to_sync(&config, &full_path, name, false)?;
        assert!(
            matches!(result, dotstate::services::sync_service::AddFileResult::Success),
            "Failed to add {}",
            name
        );
    }

    // Verify all are synced
    for name in &[".file1", ".file2", ".file3"] {
        env.assert_is_symlink(name);
        env.assert_file_tracked(name);
        env.assert_file_in_profile("default", name);
    }

    Ok(())
}
