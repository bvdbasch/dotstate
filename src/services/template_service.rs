//! Template service for template rendering operations.
//!
//! This module provides a service layer for template-related operations,
//! abstracting rendering details from the UI and CLI layers.
//!
//! A template is a committed source file in the repo that is rendered to an
//! arbitrary destination path by a built-in engine (e.g. 1Password's
//! `op inject`) or a custom user-supplied command. The rendered output is NOT
//! symlinked or tracked by `DotState` — it is produced on demand and is expected
//! to be git-ignored at its destination.

use crate::utils::path::expand_path;
use crate::utils::profile_manifest::{Template, TemplateEngine};
use crate::utils::template_engine::TemplateEngineImpl;
use crate::utils::ProfileManifest;
use anyhow::Result;
use std::io::{BufRead, BufReader};
use std::path::Path;
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
#[derive(Debug)]
pub struct TemplateCreationParams<'a> {
    pub name: &'a str,
    pub source: &'a str,
    pub destination: &'a str,
    pub engine: TemplateEngine,
    pub render_command: &'a str,
    pub description: &'a str,
}

/// Service for template-related operations.
pub struct TemplateService;

impl TemplateService {
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
        if params.source.trim().is_empty() {
            return TemplateValidation {
                is_valid: false,
                error_message: Some("Template source cannot be empty".to_string()),
            };
        }
        if params.destination.trim().is_empty() {
            return TemplateValidation {
                is_valid: false,
                error_message: Some("Template destination cannot be empty".to_string()),
            };
        }
        if params.engine == TemplateEngine::Custom && params.render_command.trim().is_empty() {
            return TemplateValidation {
                is_valid: false,
                error_message: Some("Custom engine requires a non-empty render command".to_string()),
            };
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
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        } else {
            None
        };

        let description = if params.description.trim().is_empty() {
            None
        } else {
            Some(params.description.trim().to_string())
        };

        Template {
            name: params.name.trim().to_string(),
            source: params.source.trim().to_string(),
            destination: params.destination.trim().to_string(),
            engine: params.engine,
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

    /// Get the templates resolved for a profile (walking the inheritance chain).
    pub fn get_templates(repo_path: &Path, profile_name: &str) -> Result<Vec<Template>> {
        let manifest = ProfileManifest::load_or_backfill(repo_path)?;
        manifest.resolve_templates(profile_name)
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

    /// Render a single template, executing its engine command and streaming
    /// combined stdout/stderr into the returned outcome.
    ///
    /// # Errors
    /// - If the render command cannot be built (e.g. Custom without a command).
    /// - If the child process cannot be spawned.
    pub fn render_template(repo_path: &Path, template: &Template) -> Result<RenderOutcome> {
        info!(
            "Rendering template '{}' -> {} (engine: {:?})",
            template.name, template.destination, template.engine
        );

        let mut cmd = TemplateEngineImpl::build_render_command(template, repo_path)?;

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

    fn params<'a>(name: &'a str, engine: TemplateEngine, render_command: &'a str) -> TemplateCreationParams<'a> {
        TemplateCreationParams {
            name,
            source: "git/config.tpl",
            destination: "~/.config/git/config",
            engine,
            render_command,
            description: "",
        }
    }

    #[test]
    fn test_validate_rejects_empty_name() {
        let p = params("", TemplateEngine::OpInject, "");
        assert!(!TemplateService::validate_template(&p).is_valid);
    }

    #[test]
    fn test_validate_custom_requires_command() {
        let p = params("x", TemplateEngine::Custom, "");
        let v = TemplateService::validate_template(&p);
        assert!(!v.is_valid);
        assert!(v.error_message.unwrap().contains("render command"));
    }

    #[test]
    fn test_validate_op_inject_ok_without_command() {
        let p = params("git config", TemplateEngine::OpInject, "");
        assert!(TemplateService::validate_template(&p).is_valid);
    }

    #[test]
    fn test_create_template_drops_command_for_builtin() {
        let t = TemplateService::create_template(params("git config", TemplateEngine::OpInject, "ignored"));
        assert!(t.render_command.is_none());
        assert_eq!(t.engine, TemplateEngine::OpInject);
    }

    #[test]
    fn test_create_template_keeps_command_for_custom() {
        let t = TemplateService::create_template(params(
            "custom",
            TemplateEngine::Custom,
            "tera -t {source} -o {destination}",
        ));
        assert_eq!(t.render_command.as_deref(), Some("tera -t {source} -o {destination}"));
    }

    #[test]
    fn test_available_engines_includes_custom() {
        assert!(TemplateService::available_engines().contains(&TemplateEngine::Custom));
    }

    #[test]
    fn test_check_template_missing() {
        let t = Template {
            name: "x".to_string(),
            source: "a.tpl".to_string(),
            destination: "/nonexistent/path/that/should/not/exist/xyz".to_string(),
            engine: TemplateEngine::OpInject,
            render_command: None,
            description: None,
        };
        assert_eq!(TemplateService::check_template(&t).status, TemplateStatus::Missing);
    }
}
