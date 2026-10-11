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
use crate::utils::ProfileManifest;
use anyhow::Result;
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

/// Service for template-related operations.
pub struct TemplateService;

impl TemplateService {
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

    /// Render a single resolved template, executing its engine command and
    /// streaming combined stdout/stderr into the returned outcome.
    ///
    /// # Errors
    /// - If the render command cannot be built (e.g. Custom without a command).
    /// - If the source template file is missing.
    /// - If the child process cannot be spawned.
    pub fn render_template(repo_path: &Path, resolved: &ResolvedTemplate) -> Result<RenderOutcome> {
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

        let mut output: Vec<String> = Vec::new();
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
}
