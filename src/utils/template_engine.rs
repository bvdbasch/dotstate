use crate::utils::path::expand_path;
use crate::utils::profile_manifest::{Template, TemplateEngine};
use anyhow::{anyhow, Result};
use std::process::Command;

/// Template engine implementation utilities.
///
/// Mirrors `PackageManagerImpl`: built-in engines map to a hardcoded
/// `Command` (no shell), while the `Custom` engine is the single sanctioned
/// shell path (`sh -c`) for user-supplied render commands.
pub struct TemplateEngineImpl;

impl TemplateEngineImpl {
    /// Whether the binary backing a built-in engine is available in PATH.
    ///
    /// `Custom` is always considered available (it runs via `sh -c`).
    #[must_use]
    pub fn is_engine_available(engine: &TemplateEngine) -> bool {
        use crate::utils::package_manager::PackageManagerImpl;
        let binary = match engine {
            TemplateEngine::OpInject => "op",
            TemplateEngine::Custom => return true,
        };
        PackageManagerImpl::check_binary_in_path(binary)
    }

    /// Substitute `{source}` and `{destination}` placeholders in a command string.
    #[must_use]
    pub fn substitute_placeholders(command: &str, source: &str, destination: &str) -> String {
        command
            .replace("{source}", source)
            .replace("{destination}", destination)
    }

    /// Build the render command for a template as a `std::process::Command`.
    ///
    /// Built-in engines use direct `Command::new()` (no shell injection risk).
    /// The `Custom` engine uses `sh -c` with the user-supplied `render_command`,
    /// after `{source}`/`{destination}` placeholder substitution.
    ///
    /// The `source` is resolved relative to `repo_path`; the `destination` has
    /// `~` expanded to the home directory.
    ///
    /// # Errors
    /// - If the `Custom` engine is selected but `render_command` is missing/empty.
    pub fn build_render_command(template: &Template, repo_path: &std::path::Path) -> Result<Command> {
        // Resolve source relative to the repo root.
        let source_path = repo_path.join(&template.source);
        let source = source_path.to_string_lossy().to_string();

        // Expand `~` in the destination.
        let destination = expand_path(&template.destination).to_string_lossy().to_string();

        match template.engine {
            TemplateEngine::OpInject => {
                let mut cmd = Command::new("op");
                cmd.arg("inject").arg("-i").arg(&source).arg("-o").arg(&destination);
                Ok(cmd)
            }
            TemplateEngine::Custom => {
                let raw = template
                    .render_command
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        anyhow!(
                            "Custom template '{}' requires a non-empty render_command",
                            template.name
                        )
                    })?;

                let rendered = Self::substitute_placeholders(raw, &source, &destination);

                // Custom commands use `sh -c` (the only sanctioned shell path),
                // matching how custom packages are executed.
                let mut cmd = Command::new("sh");
                cmd.arg("-c").arg(rendered);
                Ok(cmd)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::profile_manifest::Template;
    use std::path::Path;

    fn op_template() -> Template {
        Template {
            name: "git config".to_string(),
            source: "git/config.tpl".to_string(),
            destination: "~/.config/git/config".to_string(),
            engine: TemplateEngine::OpInject,
            render_command: None,
            description: None,
        }
    }

    #[test]
    fn test_substitute_placeholders() {
        let out = TemplateEngineImpl::substitute_placeholders(
            "op run -- tera -t {source} -o {destination}",
            "/repo/a.tpl",
            "/home/me/a",
        );
        assert_eq!(out, "op run -- tera -t /repo/a.tpl -o /home/me/a");
    }

    #[test]
    fn test_op_inject_command_built() {
        let tpl = op_template();
        let cmd = TemplateEngineImpl::build_render_command(&tpl, Path::new("/repo")).unwrap();
        assert_eq!(cmd.get_program(), "op");
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert_eq!(args[0], "inject");
        assert_eq!(args[1], "-i");
        assert!(args[2].ends_with("git/config.tpl"));
        assert_eq!(args[3], "-o");
    }

    #[test]
    fn test_custom_requires_render_command() {
        let mut tpl = op_template();
        tpl.engine = TemplateEngine::Custom;
        tpl.render_command = None;
        let err = TemplateEngineImpl::build_render_command(&tpl, Path::new("/repo"));
        assert!(err.is_err());
    }

    #[test]
    fn test_custom_builds_sh_c() {
        let mut tpl = op_template();
        tpl.engine = TemplateEngine::Custom;
        tpl.render_command = Some("tera -t {source} -o {destination}".to_string());
        let cmd = TemplateEngineImpl::build_render_command(&tpl, Path::new("/repo")).unwrap();
        assert_eq!(cmd.get_program(), "sh");
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert_eq!(args[0], "-c");
        assert!(args[1].contains("/repo/git/config.tpl"));
    }

    #[test]
    fn test_op_engine_availability_reflects_binary() {
        // Custom is always available regardless of environment.
        assert!(TemplateEngineImpl::is_engine_available(&TemplateEngine::Custom));
    }
}
