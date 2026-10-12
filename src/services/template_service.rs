//! Template service for template rendering operations.
//!
//! This module provides a service layer for template-related operations,
//! abstracting rendering details from the UI and CLI layers.
//!
//! A template is a committed source file that is rendered to a home-relative
//! destination by a built-in engine (e.g. 1Password's `op inject`) or a custom
//! user-supplied command. The source lives in the repo under a profile subtree,
//! `<repo>/templates/<profile>/<destination>.<ext>` (mirroring how synced files
//! live under `<repo>/<profile>/<path>`), so the source path is *derived* from
//! the destination, engine, and owning profile rather than stored. The rendered
//! output is NOT symlinked or tracked by `DotState`; it is produced on demand
//! and is expected to be git-ignored at its destination.

use crate::utils::path::expand_path;
use crate::utils::profile_manifest::{ResolvedTemplate, Template, TemplateEngine};
use crate::utils::template_engine::TemplateEngineImpl;
use crate::utils::BackupManager;
use crate::utils::ProfileManifest;
use anyhow::{Context, Result};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use tracing::{info, warn};

/// Status of a template's rendered output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateStatus {
    /// Destination file exists on disk.
    Rendered,
    /// Destination file does not exist.
    Missing,
    /// Status is unknown (not yet checked).
    Unknown,
}

/// Result of checking a template's rendered output.
#[derive(Debug)]
pub struct TemplateCheckResult {
    /// The status of the template's destination.
    pub status: TemplateStatus,
    /// The expanded destination path that was checked.
    pub destination: String,
}

/// Result of validating a template definition.
#[derive(Debug)]
pub struct TemplateValidation {
    /// Whether the template is valid.
    pub is_valid: bool,
    /// Error message if not valid.
    pub error_message: Option<String>,
}

/// Outcome of rendering a template.
#[derive(Debug)]
pub struct RenderOutcome {
    /// Whether the render command succeeded.
    pub success: bool,
    /// Combined streamed output (stdout + stderr) from the render command.
    pub output: Vec<String>,
    /// Error message if the render failed.
    pub error: Option<String>,
}

/// Parameters for creating a template.
///
/// The source path is not supplied: it is derived from the destination, engine
/// (extension), and owning profile. `extension` is only honored for the
/// `Custom` engine; built-in engines derive their extension from the engine.
#[derive(Debug)]
pub struct TemplateCreationParams<'a> {
    pub name: &'a str,
    pub destination: &'a str,
    pub engine: TemplateEngine,
    pub extension: &'a str,
    pub render_command: &'a str,
    pub description: &'a str,
}

/// A source file found under `templates/` that has no matching manifest entry.
///
/// Discovery reverses the on-disk layout `templates/<profile>/<rel>.<ext>` back
/// into the fields needed to pre-fill the Add form. A file placed directly in
/// `templates/<rel>.<ext>` (no profile segment) is attributed to the active
/// profile, mirroring how file discovery assigns ambiguous files.
#[derive(Debug, Clone)]
pub struct DiscoveredTemplate {
    /// Source path relative to the repo root (e.g. `templates/common/.config/git/work.tpl`).
    pub source_relative: String,
    /// Owning profile the entry will be created under once adopted.
    pub source_profile: String,
    /// Derived template name (file stem, no extension).
    pub name: String,
    /// Derived home-relative destination (e.g. `.config/git/work`).
    pub destination: String,
    /// The file's extension without the leading dot (empty if none).
    pub extension: String,
    /// Engine inferred from the extension (`tpl` -> op-inject; otherwise custom).
    pub inferred_engine: TemplateEngine,
}

/// Service for template-related operations.
pub struct TemplateService;

impl TemplateService {
    /// Register a discovered source under its owning profile or common section.
    /// Relocate it only if its canonical source path differs; roll back the move
    /// if persisting the manifest fails.
    pub fn adopt_template(
        repo_path: &Path,
        source_relative: &str,
        source_profile: &str,
        template: Template,
    ) -> Result<()> {
        use std::path::Component;
        let validation = Self::validate_template(&TemplateCreationParams {
            name: &template.name,
            destination: &template.destination,
            engine: template.engine.clone(),
            extension: template.extension.as_deref().unwrap_or(""),
            render_command: template.render_command.as_deref().unwrap_or(""),
            description: template.description.as_deref().unwrap_or(""),
        });
        anyhow::ensure!(validation.is_valid, "{}", validation.error_message.unwrap_or_default());
        let is_relative =
            |path: &str| !path.is_empty() && Path::new(path).components().all(|c| matches!(c, Component::Normal(_)));
        anyhow::ensure!(
            is_relative(&template.destination),
            "Destination must be home-relative without '..'"
        );
        anyhow::ensure!(
            is_relative(source_relative) && Path::new(source_relative).starts_with("templates"),
            "Source must be inside templates/"
        );
        let extension = template.source_extension();
        anyhow::ensure!(
            !extension.contains(['/', '\\']) && extension != "..",
            "Invalid source extension"
        );

        let mut manifest = ProfileManifest::load_or_backfill(repo_path)?;
        let entries = if source_profile == "common" {
            &mut manifest.common.templates
        } else {
            &mut manifest
                .profiles
                .iter_mut()
                .find(|p| p.name == source_profile)
                .ok_or_else(|| anyhow::anyhow!("Profile '{source_profile}' not found"))?
                .templates
        };
        anyhow::ensure!(
            !entries
                .iter()
                .any(|t| t.name == template.name || t.destination == template.destination),
            "A template already uses that name or destination in '{source_profile}'"
        );
        let from = repo_path.join(source_relative);
        let to = Self::source_abs_path(repo_path, source_profile, &template);
        anyhow::ensure!(
            std::fs::symlink_metadata(&from)?.file_type().is_file(),
            "Source must be a regular file"
        );
        let root = repo_path.join("templates").canonicalize()?;
        anyhow::ensure!(from.canonicalize()?.starts_with(&root), "Source escapes templates/");
        let moved = from != to;
        if moved {
            anyhow::ensure!(
                std::fs::symlink_metadata(&to).is_err(),
                "A template source already exists at {}",
                to.display()
            );
            let parent = to.parent().ok_or_else(|| anyhow::anyhow!("Invalid source path"))?;
            std::fs::create_dir_all(parent)?;
            anyhow::ensure!(
                parent.canonicalize()?.starts_with(&root),
                "Destination source escapes templates/"
            );
            std::fs::rename(&from, &to)?;
        }
        entries.push(template);
        if let Err(error) = manifest.save(repo_path) {
            if moved {
                std::fs::rename(&to, &from)
                    .with_context(|| format!("Manifest save failed ({error}); source rollback failed"))?;
            }
            return Err(error);
        }
        Ok(())
    }

    /// Discover source files under `templates/` that are not yet recorded in the
    /// manifest for the active profile's resolved view.
    ///
    /// Scans the entire `templates/` tree. Each file maps to a profile segment
    /// (`templates/<profile>/...`) or, when placed directly under `templates/`,
    /// is attributed to `active_profile`. A file is "tracked" if some template
    /// in the manifest derives the same source path; everything else is
    /// returned as a `DiscoveredTemplate`, sorted by source path.
    pub fn discover_untracked_templates(repo_path: &Path, active_profile: &str) -> Result<Vec<DiscoveredTemplate>> {
        let templates_root = repo_path.join("templates");
        if !templates_root.is_dir() {
            return Ok(Vec::new());
        }

        // Collect every source path already derivable from the manifest so we
        // can exclude tracked files regardless of which profile owns them.
        let manifest = ProfileManifest::load_or_backfill(repo_path)?;
        let mut tracked: std::collections::HashSet<String> = std::collections::HashSet::new();
        for profile in &manifest.profiles {
            for tpl in &profile.templates {
                tracked.insert(tpl.source_relative_path(&profile.name));
            }
        }
        for tpl in &manifest.common.templates {
            tracked.insert(tpl.source_relative_path("common"));
        }

        let mut files: Vec<String> = Vec::new();
        Self::collect_files_recursive(&templates_root, &templates_root, &mut files)?;

        let mut discovered = Vec::new();
        for rel_under_templates in files {
            // `rel_under_templates` is relative to the `templates/` dir.
            let source_relative = format!("templates/{rel_under_templates}");
            if tracked.contains(&source_relative) {
                continue;
            }

            // Determine owning profile + the remaining path.
            // `<profile>/<rest>` — if the first segment is a known profile or
            // "common", use it; otherwise the file sits at the templates root
            // (or under an unknown folder) and is attributed to the active
            // profile with its full relative path as the destination base.
            let (owning_profile, rest) = Self::split_profile_segment(&rel_under_templates, &manifest, active_profile);

            let (destination, extension) = Self::split_destination_extension(&rest);
            if destination.is_empty() {
                continue;
            }

            let inferred_engine = if extension == "tpl" {
                TemplateEngine::OpInject
            } else {
                TemplateEngine::Custom
            };

            let name = std::path::Path::new(&destination)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("template")
                .trim_start_matches('.')
                .to_string();

            discovered.push(DiscoveredTemplate {
                source_relative,
                source_profile: owning_profile,
                name,
                destination,
                extension,
                inferred_engine,
            });
        }

        discovered.sort_by(|a, b| a.source_relative.cmp(&b.source_relative));
        Ok(discovered)
    }

    /// Split a `templates/`-relative path into `(owning_profile, rest_path)`.
    ///
    /// If the first path segment names an existing profile or is `common`, that
    /// segment owns the file. Otherwise the file is attributed to the active
    /// profile and the entire path is treated as the destination base.
    fn split_profile_segment(rel: &str, manifest: &ProfileManifest, active_profile: &str) -> (String, String) {
        let mut parts = rel.splitn(2, '/');
        let first = parts.next().unwrap_or("");
        let remainder = parts.next();

        let is_profile = first == "common" || manifest.profiles.iter().any(|p| p.name == first);
        match (is_profile, remainder) {
            (true, Some(rest)) if !rest.is_empty() => (first.to_string(), rest.to_string()),
            // Not a profile segment: attribute to active profile, keep full path.
            _ => (active_profile.to_string(), rel.to_string()),
        }
    }

    /// Split a relative source path (no `templates/<profile>/` prefix) into a
    /// home-relative `(destination, extension)` by stripping the final suffix.
    fn split_destination_extension(rest: &str) -> (String, String) {
        let path = std::path::Path::new(rest);
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) if !ext.is_empty() => {
                let without_ext = rest.strip_suffix(&format!(".{ext}")).unwrap_or(rest).to_string();
                (without_ext, ext.to_string())
            }
            _ => (rest.to_string(), String::new()),
        }
    }

    /// Recursively collect files under `dir`, returning paths relative to `base`.
    fn collect_files_recursive(base: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            // Do not follow symlinks: directory links may loop or leave the repo.
            if kind.is_dir() {
                Self::collect_files_recursive(base, &path, out)?;
            } else if kind.is_file() {
                if let Ok(rel) = path.strip_prefix(base) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        Ok(())
    }

    /// Absolute path to a template's source file in the repo, given the profile
    /// that owns it (`templates/<source_profile>/<destination>.<ext>`).
    #[must_use]
    pub fn source_abs_path(repo_path: &Path, source_profile: &str, template: &Template) -> PathBuf {
        repo_path.join(template.source_relative_path(source_profile))
    }

    /// Create the source template file if it does not already exist, including
    /// parent directories. Returns `true` if a new stub was created.
    pub fn ensure_source_stub(repo_path: &Path, source_profile: &str, template: &Template) -> Result<bool> {
        let path = Self::source_abs_path(repo_path, source_profile, template);
        if path.exists() {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, "")?;
        info!("Created template source stub: {}", path.display());
        Ok(true)
    }

    /// Import an existing on-disk file as this template's source, moving it into
    /// the profile's template subtree with the engine's extension appended. The
    /// file contents are preserved exactly (no templating applied).
    ///
    /// The original file is removed after a successful copy, mirroring how
    /// syncing a file relocates it into the repo. The rendered destination is
    /// NOT created here — rendering stays an explicit, separate step.
    ///
    /// # Errors
    /// - If the source file does not exist or is not a regular file.
    /// - If a source template already exists at the derived path.
    pub fn import_source_file(
        repo_path: &Path,
        source_profile: &str,
        template: &Template,
        from_file: &Path,
    ) -> Result<()> {
        if !from_file.is_file() {
            return Err(anyhow::anyhow!(
                "Cannot import '{}': not a regular file",
                from_file.display()
            ));
        }
        let dest = Self::source_abs_path(repo_path, source_profile, template);
        if dest.exists() {
            return Err(anyhow::anyhow!(
                "A template source already exists at {}",
                dest.display()
            ));
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Copy first (non-destructive), then remove the original.
        std::fs::copy(from_file, &dest)
            .with_context(|| format!("Failed to copy {} to {}", from_file.display(), dest.display()))?;
        if let Err(e) = std::fs::remove_file(from_file) {
            warn!(
                "Imported template source copied to {}, but failed to remove original {}: {e}",
                dest.display(),
                from_file.display()
            );
        }
        info!(
            "Imported '{}' as template source {}",
            from_file.display(),
            dest.display()
        );
        Ok(())
    }

    /// Update an owned template by its index in the profile (not the resolved list).
    pub fn update_template(repo_path: &Path, profile_name: &str, index: usize, template: Template) -> Result<()> {
        let mut manifest = ProfileManifest::load_or_backfill(repo_path)?;
        let profile = manifest
            .profiles
            .iter_mut()
            .find(|p| p.name == profile_name)
            .ok_or_else(|| anyhow::anyhow!("Profile '{profile_name}' not found"))?;
        let entry = profile
            .templates
            .get_mut(index)
            .ok_or_else(|| anyhow::anyhow!("Template index {index} out of bounds"))?;
        *entry = template;
        manifest.save(repo_path)
    }

    /// Get the built-in template engines available on this system.
    ///
    /// `Custom` is always available; built-in engines require their backing
    /// binary to be present in PATH.
    #[must_use]
    pub fn available_engines() -> Vec<TemplateEngine> {
        [TemplateEngine::OpInject, TemplateEngine::Custom]
            .into_iter()
            .filter(TemplateEngineImpl::is_engine_available)
            .collect()
    }

    /// Validate a template definition before it is added.
    #[must_use]
    pub fn validate_template(params: &TemplateCreationParams) -> TemplateValidation {
        if params.name.trim().is_empty() {
            return TemplateValidation {
                is_valid: false,
                error_message: Some("Template name cannot be empty".to_string()),
            };
        }
        // Names must not carry an extension; the engine decides that.
        if params.name.trim().contains('.') {
            return TemplateValidation {
                is_valid: false,
                error_message: Some("Template name must not include a file extension".to_string()),
            };
        }
        if params.destination.trim().is_empty() {
            return TemplateValidation {
                is_valid: false,
                error_message: Some("Template destination cannot be empty".to_string()),
            };
        }
        if params.engine == TemplateEngine::Custom {
            if params.render_command.trim().is_empty() {
                return TemplateValidation {
                    is_valid: false,
                    error_message: Some("Custom engine requires a non-empty render command".to_string()),
                };
            }
            if params.extension.trim().is_empty() {
                return TemplateValidation {
                    is_valid: false,
                    error_message: Some("Custom engine requires a source file extension".to_string()),
                };
            }
        }

        TemplateValidation {
            is_valid: true,
            error_message: None,
        }
    }

    /// Build a `Template` from creation parameters.
    #[must_use]
    pub fn create_template(params: TemplateCreationParams) -> Template {
        let render_command = if params.engine == TemplateEngine::Custom {
            let trimmed = params.render_command.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        } else {
            None
        };

        // Only the Custom engine stores an extension; built-ins derive it.
        let extension = if params.engine == TemplateEngine::Custom {
            let trimmed = params.extension.trim().trim_start_matches('.');
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        } else {
            None
        };

        let description = {
            let trimmed = params.description.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };

        Template {
            name: params.name.trim().to_string(),
            destination: params.destination.trim().trim_start_matches('/').to_string(),
            engine: params.engine,
            extension,
            render_command,
            description,
        }
    }

    /// Add a template to a profile.
    ///
    /// Returns the updated list of templates for the profile.
    pub fn add_template(repo_path: &Path, profile_name: &str, template: Template) -> Result<Vec<Template>> {
        info!("Adding new template: {} (profile: {})", template.name, profile_name);

        let mut manifest = ProfileManifest::load_or_backfill(repo_path)?;

        if let Some(profile) = manifest.profiles.iter_mut().find(|p| p.name == profile_name) {
            profile.templates.push(template);
            let templates = profile.templates.clone();
            manifest.save(repo_path)?;
            Ok(templates)
        } else {
            Err(anyhow::anyhow!("Profile '{profile_name}' not found"))
        }
    }

    /// Delete a template from a profile by index.
    ///
    /// Returns the updated list of templates for the profile.
    pub fn delete_template(repo_path: &Path, profile_name: &str, index: usize) -> Result<Vec<Template>> {
        let mut manifest = ProfileManifest::load_or_backfill(repo_path)?;

        if let Some(profile) = manifest.profiles.iter_mut().find(|p| p.name == profile_name) {
            if index < profile.templates.len() {
                let template_name = profile.templates[index].name.clone();
                info!(
                    "Deleting template: {} (index: {}, profile: {})",
                    template_name, index, profile_name
                );
                profile.templates.remove(index);
                let templates = profile.templates.clone();
                manifest.save(repo_path)?;
                Ok(templates)
            } else {
                Err(anyhow::anyhow!(
                    "Template index {} out of bounds ({} templates)",
                    index,
                    profile.templates.len()
                ))
            }
        } else {
            Err(anyhow::anyhow!("Profile '{profile_name}' not found"))
        }
    }

    /// Get the templates resolved for a profile (walking the inheritance chain
    /// and merging common templates).
    pub fn get_templates(repo_path: &Path, profile_name: &str) -> Result<Vec<ResolvedTemplate>> {
        let manifest = ProfileManifest::load_or_backfill(repo_path)?;
        manifest.resolve_templates(profile_name)
    }

    /// Get a profile's own templates (unsorted, as stored in the manifest).
    ///
    /// Useful for resolving a stable index into the manifest list for
    /// update/delete, independent of the sorted resolved view.
    pub fn profile_templates(repo_path: &Path, profile_name: &str) -> Result<Vec<Template>> {
        let manifest = ProfileManifest::load_or_backfill(repo_path)?;
        Ok(manifest
            .profiles
            .into_iter()
            .find(|p| p.name == profile_name)
            .map(|p| p.templates)
            .unwrap_or_default())
    }

    /// Check whether a template's destination file exists on disk.
    #[must_use]
    pub fn check_template(template: &Template) -> TemplateCheckResult {
        let destination = expand_path(&template.destination);
        let status = if destination.exists() {
            TemplateStatus::Rendered
        } else {
            TemplateStatus::Missing
        };

        TemplateCheckResult {
            status,
            destination: destination.to_string_lossy().to_string(),
        }
    }

    /// Render a single resolved template, backing up and removing any existing
    /// destination before executing its engine command. This prevents engines
    /// such as `op inject` from blocking on their overwrite prompt.
    ///
    /// # Errors
    /// - If the render command cannot be built (e.g. Custom without a command).
    /// - If the source template file is missing.
    /// - If the child process cannot be spawned.
    pub fn render_template(
        repo_path: &Path,
        resolved: &ResolvedTemplate,
        backup_enabled: bool,
    ) -> Result<RenderOutcome> {
        let template = &resolved.template;
        let source_abs = Self::source_abs_path(repo_path, &resolved.source_profile, template);

        info!(
            "Rendering template '{}' ({} -> {}) engine={:?}",
            template.name,
            source_abs.display(),
            template.destination,
            template.engine
        );

        if !source_abs.exists() {
            return Ok(RenderOutcome {
                success: false,
                output: Vec::new(),
                error: Some(format!("Source template file is missing: {}", source_abs.display())),
            });
        }

        let destination = expand_path(&template.destination);
        let backup_manager = backup_enabled.then(BackupManager::new).transpose()?;
        let backup = Self::backup_and_remove_destination(&destination, backup_manager.as_ref())?;
        let mut output: Vec<String> = backup
            .map(|path| format!("Backed up existing destination to {}", path.display()))
            .into_iter()
            .collect();

        let mut cmd = TemplateEngineImpl::build_render_command(template, &source_abs)?;

        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let (tx, rx) = mpsc::channel::<String>();
        let tx_stdout = tx.clone();
        let tx_stderr = tx;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture stdout"))?;
        let stdout_handle = thread::spawn(move || {
            let reader = BufReader::new(stdout);
            #[allow(clippy::lines_filter_map_ok)]
            for line in reader.lines().flatten() {
                let _ = tx_stdout.send(line);
            }
        });

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture stderr"))?;
        let stderr_handle = thread::spawn(move || {
            let reader = BufReader::new(stderr);
            #[allow(clippy::lines_filter_map_ok)]
            for line in reader.lines().flatten() {
                let _ = tx_stderr.send(format!("[stderr] {line}"));
            }
        });

        for line in rx {
            output.push(line);
        }

        let _ = stdout_handle.join();
        let _ = stderr_handle.join();

        let status = child.wait()?;
        let success = status.success();
        let error = if success {
            None
        } else {
            warn!("Template '{}' render failed", template.name);
            Some(format!(
                "Render command exited with status {}",
                status.code().map_or_else(|| "unknown".to_string(), |c| c.to_string())
            ))
        };

        Ok(RenderOutcome { success, output, error })
    }

    /// Back up and remove an existing file so non-interactive render commands
    /// can write their output. Directories are deliberately refused because a
    /// template destination represents a single file.
    fn backup_and_remove_destination(
        destination: &Path,
        backup_manager: Option<&BackupManager>,
    ) -> Result<Option<PathBuf>> {
        let metadata = match std::fs::symlink_metadata(destination) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to inspect template destination {}", destination.display()))
            }
        };
        anyhow::ensure!(
            !metadata.file_type().is_dir(),
            "Template destination {} is a directory; refusing to replace it",
            destination.display()
        );

        let backup = if let Some(manager) = backup_manager {
            let session = manager.create_backup_session()?;
            let relative_name = Self::backup_relative_name(destination)?;
            Some(manager.backup_path(&session, destination, &relative_name)?)
        } else {
            None
        };
        std::fs::remove_file(destination).with_context(|| {
            format!(
                "Failed to remove existing template destination {}",
                destination.display()
            )
        })?;
        Ok(backup)
    }

    /// Return a safe path for a destination in a centralized backup session.
    /// Home-relative destinations retain their layout; external absolute paths
    /// live under `external/` and can never cause `Path::join` to escape it.
    fn backup_relative_name(destination: &Path) -> Result<String> {
        let destination = destination.canonicalize()?;
        let home = crate::utils::get_home_dir().canonicalize()?;
        let relative = destination.strip_prefix(&home).map(PathBuf::from).unwrap_or_else(|_| {
            let without_root = destination.strip_prefix("/").unwrap_or(&destination);
            Path::new("external").join(without_root)
        });
        anyhow::ensure!(
            relative
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
            "Template destination cannot be represented safely in a backup"
        );
        Ok(relative.to_string_lossy().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params<'a>(
        name: &'a str,
        engine: TemplateEngine,
        render_command: &'a str,
        extension: &'a str,
    ) -> TemplateCreationParams<'a> {
        TemplateCreationParams {
            name,
            destination: ".config/git/config",
            engine,
            extension,
            render_command,
            description: "",
        }
    }

    #[test]
    fn test_validate_rejects_empty_name() {
        let p = params("", TemplateEngine::OpInject, "", "");
        assert!(!TemplateService::validate_template(&p).is_valid);
    }

    #[test]
    fn test_validate_rejects_name_with_extension() {
        let p = params("config.tpl", TemplateEngine::OpInject, "", "");
        let v = TemplateService::validate_template(&p);
        assert!(!v.is_valid);
        assert!(v.error_message.unwrap().contains("extension"));
    }

    #[test]
    fn test_validate_custom_requires_command_and_extension() {
        let no_cmd = params("x", TemplateEngine::Custom, "", "j2");
        assert!(!TemplateService::validate_template(&no_cmd).is_valid);
        let no_ext = params("x", TemplateEngine::Custom, "tera", "");
        let v = TemplateService::validate_template(&no_ext);
        assert!(!v.is_valid);
        assert!(v.error_message.unwrap().contains("extension"));
    }

    #[test]
    fn test_validate_op_inject_ok_without_command() {
        let p = params("config", TemplateEngine::OpInject, "", "");
        assert!(TemplateService::validate_template(&p).is_valid);
    }

    #[test]
    fn test_create_template_drops_command_and_extension_for_builtin() {
        let t = TemplateService::create_template(params("config", TemplateEngine::OpInject, "ignored", "ignored"));
        assert!(t.render_command.is_none());
        assert!(t.extension.is_none());
        assert_eq!(t.engine, TemplateEngine::OpInject);
        // Built-in extension derives from the engine.
        assert_eq!(t.source_extension(), "tpl");
    }

    #[test]
    fn test_create_template_keeps_command_and_extension_for_custom() {
        let t = TemplateService::create_template(params(
            "config",
            TemplateEngine::Custom,
            "tera -t {source} -o {destination}",
            ".j2",
        ));
        assert_eq!(t.render_command.as_deref(), Some("tera -t {source} -o {destination}"));
        // Leading dot is stripped.
        assert_eq!(t.extension.as_deref(), Some("j2"));
        assert_eq!(t.source_extension(), "j2");
    }

    #[test]
    fn test_source_relative_path_mirrors_destination_under_profile() {
        let t = TemplateService::create_template(params("config", TemplateEngine::OpInject, "", ""));
        assert_eq!(
            t.source_relative_path("personal"),
            "templates/personal/.config/git/config.tpl"
        );
    }

    #[test]
    fn test_create_template_strips_leading_slash_from_destination() {
        let mut p = params("config", TemplateEngine::OpInject, "", "");
        p.destination = "/.config/git/config";
        let t = TemplateService::create_template(p);
        assert_eq!(t.destination, ".config/git/config");
    }

    #[test]
    fn test_available_engines_includes_custom() {
        assert!(TemplateService::available_engines().contains(&TemplateEngine::Custom));
    }

    #[test]
    fn test_check_template_missing() {
        let t = Template {
            name: "x".to_string(),
            destination: "/nonexistent/path/that/should/not/exist/xyz".to_string(),
            engine: TemplateEngine::OpInject,
            extension: None,
            render_command: None,
            description: None,
        };
        assert_eq!(TemplateService::check_template(&t).status, TemplateStatus::Missing);
    }

    #[test]
    fn test_import_source_file_moves_and_preserves_contents() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path();
        // An on-disk file to convert into a template source.
        let src = repo.join("on-disk.conf");
        std::fs::write(&src, "key = value\n").unwrap();

        let tpl = TemplateService::create_template(params("gitwork", TemplateEngine::OpInject, "", ""));
        TemplateService::import_source_file(repo, "personal", &tpl, &src).unwrap();

        // Original is gone; source lives under the profile subtree with .tpl ext.
        assert!(!src.exists(), "original file should be moved");
        let dest = repo.join("templates/personal/.config/git/config.tpl");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "key = value\n");
    }

    #[test]
    fn test_import_source_file_rejects_existing_source() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path();
        let src = repo.join("on-disk.conf");
        std::fs::write(&src, "data\n").unwrap();
        let tpl = TemplateService::create_template(params("gitwork", TemplateEngine::OpInject, "", ""));
        // Pre-create the derived source so the import collides.
        TemplateService::ensure_source_stub(repo, "personal", &tpl).unwrap();
        assert!(TemplateService::import_source_file(repo, "personal", &tpl, &src).is_err());
        // Original must be left intact on failure.
        assert!(src.exists());
    }

    fn write_src(repo: &Path, rel: &str, contents: &str) {
        let path = repo.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn test_discover_untracked_templates_maps_profiles_and_infers_engine() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path();

        let mut manifest = ProfileManifest::default();
        manifest.add_profile("personal".to_string(), None);
        manifest.save(repo).unwrap();

        // Hand-authored sources:
        //  - under a known profile folder (op-inject via .tpl)
        write_src(repo, "templates/personal/.config/git/work.tpl", "{{ op://v/i/f }}");
        //  - under common with a custom extension (.j2 -> custom)
        write_src(repo, "templates/common/.config/app/conf.j2", "{{ x }}");
        //  - directly at templates root, no profile segment -> active profile
        write_src(repo, "templates/.config/root.tpl", "root");

        let found = TemplateService::discover_untracked_templates(repo, "personal").unwrap();
        assert_eq!(found.len(), 3);

        let work = found.iter().find(|d| d.source_relative.ends_with("work.tpl")).unwrap();
        assert_eq!(work.source_profile, "personal");
        assert_eq!(work.destination, ".config/git/work");
        assert_eq!(work.extension, "tpl");
        assert_eq!(work.inferred_engine, TemplateEngine::OpInject);
        assert_eq!(work.name, "work");

        let conf = found.iter().find(|d| d.source_relative.ends_with("conf.j2")).unwrap();
        assert_eq!(conf.source_profile, "common");
        assert_eq!(conf.destination, ".config/app/conf");
        assert_eq!(conf.extension, "j2");
        assert_eq!(conf.inferred_engine, TemplateEngine::Custom);

        // Root-level file with no profile segment is attributed to the active profile.
        let root = found
            .iter()
            .find(|d| d.source_relative == "templates/.config/root.tpl")
            .unwrap();
        assert_eq!(root.source_profile, "personal");
        assert_eq!(root.destination, ".config/root");
    }

    #[test]
    fn test_discover_excludes_tracked_sources() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path();

        let mut manifest = ProfileManifest::default();
        manifest.add_profile("personal".to_string(), None);
        manifest.save(repo).unwrap();

        // Register one template via the service (writes manifest + stub).
        let tpl = TemplateService::create_template(params("work", TemplateEngine::OpInject, "", ""));
        // params() uses destination ".config/git/config"
        TemplateService::ensure_source_stub(repo, "personal", &tpl).unwrap();
        TemplateService::add_template(repo, "personal", tpl).unwrap();

        // Also drop an untracked one.
        write_src(repo, "templates/personal/.config/other.tpl", "x");

        let found = TemplateService::discover_untracked_templates(repo, "personal").unwrap();
        // Only the untracked file is reported.
        assert_eq!(found.len(), 1);
        assert!(found[0].source_relative.ends_with(".config/other.tpl"));
    }

    #[test]
    fn test_discover_empty_when_no_templates_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let found = TemplateService::discover_untracked_templates(dir.path(), "personal").unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn adoption_rejects_collisions_and_invalid_owner_without_moving_source() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path();
        let mut manifest = ProfileManifest::default();
        manifest.add_profile("personal".to_string(), None);
        manifest.save(repo).unwrap();
        write_src(repo, "templates/input.tpl", "original\n");
        let tpl = TemplateService::create_template(params("config", TemplateEngine::OpInject, "", ""));
        assert!(TemplateService::adopt_template(repo, "templates/input.tpl", "missing", tpl.clone()).is_err());
        write_src(repo, "templates/personal/.config/git/config.tpl", "keep\n");
        assert!(TemplateService::adopt_template(repo, "templates/input.tpl", "personal", tpl).is_err());
        assert_eq!(
            std::fs::read_to_string(repo.join("templates/input.tpl")).unwrap(),
            "original\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("templates/personal/.config/git/config.tpl")).unwrap(),
            "keep\n"
        );
    }

    #[test]
    fn adoption_rolls_back_relocation_when_manifest_save_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path();
        let mut manifest = ProfileManifest::default();
        manifest.add_profile("personal".to_string(), None);
        manifest.save(repo).unwrap();
        write_src(repo, "templates/input.tpl", "original\n");
        // The atomic-save temporary path is blocked; load still succeeds.
        std::fs::create_dir(repo.join(".dotstate-profiles.toml.tmp")).unwrap();
        let tpl = TemplateService::create_template(params("config", TemplateEngine::OpInject, "", ""));
        assert!(TemplateService::adopt_template(repo, "templates/input.tpl", "personal", tpl).is_err());
        assert_eq!(
            std::fs::read_to_string(repo.join("templates/input.tpl")).unwrap(),
            "original\n"
        );
        assert!(!repo.join("templates/personal/.config/git/config.tpl").exists());
        assert_eq!(ProfileManifest::load(repo).unwrap().profiles[0].templates.len(), 0);
    }

    #[test]
    fn existing_render_destination_is_backed_up_then_removed() {
        let dir = tempfile::TempDir::new().unwrap();
        let destination = dir.path().join("home/.config/app/config");
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::write(&destination, "old contents\n").unwrap();
        let backup_root = dir.path().join("backups");
        std::fs::create_dir_all(&backup_root).unwrap();
        let manager = BackupManager::new_with_backup_root(backup_root.clone());

        let backup = TemplateService::backup_and_remove_destination(&destination, Some(&manager))
            .unwrap()
            .unwrap();

        assert!(!destination.exists());
        assert!(backup.starts_with(&backup_root));
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "old contents\n");
    }

    #[test]
    fn existing_directory_is_not_replaced_by_template_render() {
        let dir = tempfile::TempDir::new().unwrap();
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&destination).unwrap();

        let error = TemplateService::backup_and_remove_destination(&destination, None).unwrap_err();

        assert!(error.to_string().contains("is a directory"));
        assert!(destination.is_dir());
    }
}
