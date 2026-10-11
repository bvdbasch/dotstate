//! Template management CLI commands.
//!
//! This module provides CLI commands for managing templates in profiles:
//! - `list` - List templates for a profile (with rendered/missing status)
//! - `add` - Add a template to a profile
//! - `remove` - Remove a template from a profile
//! - `render` - Render one or all templates for a profile
//! - `check` - Check whether template destinations exist
//! - `help` - Show help for templates commands

use crate::cli::common::{
    print_error, print_info, print_success, prompt_confirm, prompt_select, prompt_select_with_suffix, prompt_string,
    prompt_string_optional, CliContext,
};
use crate::services::{TemplateCreationParams, TemplateService, TemplateStatus};
use crate::utils::profile_manifest::TemplateEngine;
use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum TemplatesCommand {
    /// List templates for a profile
    List {
        /// Target profile (defaults to active profile)
        #[arg(short, long)]
        profile: Option<String>,
        /// Show detailed template information
        #[arg(short, long)]
        verbose: bool,
    },
    /// Add a template to a profile
    Add {
        /// Target profile (defaults to active profile)
        #[arg(short, long)]
        profile: Option<String>,
        /// Template display name (no file extension)
        #[arg(short, long)]
        name: Option<String>,
        /// Destination path for the rendered file, relative to home (e.g. ".config/git/config")
        #[arg(short, long)]
        destination: Option<String>,
        /// Template engine (op-inject, custom)
        #[arg(short, long)]
        engine: Option<String>,
        /// Source file extension for the custom engine (e.g. "j2")
        #[arg(short = 'x', long)]
        extension: Option<String>,
        /// Render command (required for custom engine; supports {source}/{destination})
        #[arg(long)]
        render_command: Option<String>,
        /// Optional description
        #[arg(long)]
        description: Option<String>,
    },
    /// Remove a template from a profile
    Remove {
        /// Target profile (defaults to active profile)
        #[arg(short, long)]
        profile: Option<String>,
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
        /// Template name to remove
        name: Option<String>,
    },
    /// Render templates for a profile
    Render {
        /// Target profile (defaults to active profile)
        #[arg(short, long)]
        profile: Option<String>,
        /// Render all templates (default when no name is given)
        #[arg(short, long)]
        all: bool,
        /// Name of a single template to render
        name: Option<String>,
    },
    /// Check whether template destinations exist
    Check {
        /// Target profile (defaults to active profile)
        #[arg(short, long)]
        profile: Option<String>,
    },
    /// Show help for templates commands
    Help {
        /// Command to show help for
        command: Option<String>,
    },
}

/// Execute a templates subcommand.
pub fn execute(command: TemplatesCommand) -> Result<()> {
    match command {
        TemplatesCommand::List { profile, verbose } => cmd_list(profile, verbose),
        TemplatesCommand::Add {
            profile,
            name,
            destination,
            engine,
            extension,
            render_command,
            description,
        } => cmd_add(
            profile,
            name,
            destination,
            engine,
            extension,
            render_command,
            description,
        ),
        TemplatesCommand::Remove { profile, yes, name } => cmd_remove(profile, yes, name),
        TemplatesCommand::Render { profile, all, name } => cmd_render(profile, all, name),
        TemplatesCommand::Check { profile } => cmd_check(profile),
        TemplatesCommand::Help { command } => cmd_help(command),
    }
}

/// Human-readable name for an engine.
fn engine_str(engine: &TemplateEngine) -> &'static str {
    match engine {
        TemplateEngine::OpInject => "op-inject",
        TemplateEngine::Custom => "custom",
    }
}

/// Parse an engine name from a string (case-insensitive).
fn parse_engine(s: &str) -> Option<TemplateEngine> {
    match s.trim().to_lowercase().replace('_', "-").as_str() {
        "op-inject" | "op" | "opinject" => Some(TemplateEngine::OpInject),
        "custom" => Some(TemplateEngine::Custom),
        _ => None,
    }
}

/// Prompt the user to select a template engine.
fn prompt_engine() -> Result<TemplateEngine> {
    let options = ["op-inject", "custom"];
    let selected = prompt_select("Engine", &options)?;
    Ok(match selected {
        1 => TemplateEngine::Custom,
        _ => TemplateEngine::OpInject,
    })
}

fn cmd_help(command: Option<String>) -> Result<()> {
    match command.as_deref() {
        Some("list") => println!("dotstate templates list [--profile <name>] [--verbose]"),
        Some("add") => println!(
            "dotstate templates add [--profile <name>] --name <name> --destination <rel-path> \\\n  --engine <op-inject|custom> [--extension <ext>] [--render-command <cmd>] [--description <text>]"
        ),
        Some("remove") => {
            println!("dotstate templates remove [--profile <name>] [--yes] [<name>]");
        }
        Some("render") => {
            println!("dotstate templates render [--profile <name>] [--all] [<name>]");
        }
        Some("check") => println!("dotstate templates check [--profile <name>]"),
        _ => {
            println!("Manage templates for profiles.\n");
            println!("Templates are committed source files rendered to arbitrary destinations");
            println!("by an engine (e.g. 1Password's op inject). The rendered output is NOT");
            println!("tracked or symlinked by DotState — it is produced on demand and should");
            println!("be git-ignored at its destination.\n");
            println!("Commands:");
            println!("  list      List templates for a profile");
            println!("  add       Add a template to a profile");
            println!("  remove    Remove a template from a profile");
            println!("  render    Render one or all templates");
            println!("  check     Check whether template destinations exist");
            println!("\nEngines:");
            println!("  op-inject  Render via `op inject -i <source> -o <destination>`");
            println!("  custom     Run a user-supplied command ({{source}}/{{destination}})");
        }
    }
    Ok(())
}

fn cmd_list(profile: Option<String>, verbose: bool) -> Result<()> {
    let ctx = CliContext::load()?;
    let profile_name = ctx.resolve_profile(profile.as_deref());

    if !ctx.profile_exists(&profile_name) {
        print_error(&format!("Profile '{profile_name}' not found"));
        std::process::exit(1);
    }

    let templates = TemplateService::get_templates(&ctx.config.repo_path, &profile_name)?;

    if templates.is_empty() {
        println!("No templates configured for profile '{profile_name}'");
        println!("Use 'dotstate templates add' to add templates.");
        return Ok(());
    }

    println!("Templates for profile '{profile_name}':\n");

    let mut rendered_count = 0;
    let mut missing_count = 0;

    for resolved in &templates {
        let template = &resolved.template;
        let engine = engine_str(&template.engine);
        let check = TemplateService::check_template(template);
        let status_str = match check.status {
            TemplateStatus::Rendered => {
                rendered_count += 1;
                "\u{2713} rendered".to_string()
            }
            TemplateStatus::Missing => {
                missing_count += 1;
                "\u{2717} missing".to_string()
            }
            TemplateStatus::Unknown => "? unknown".to_string(),
        };

        let origin = if resolved.source_profile == profile_name {
            String::new()
        } else {
            format!(" ({})", resolved.source_profile)
        };

        if verbose {
            println!("  {}{origin}", template.name);
            println!("    Engine: {engine}");
            println!("    Source: {}", resolved.source_relative_path());
            println!("    Destination: {}", template.destination);
            if let Some(ref cmd) = template.render_command {
                println!("    Render command: {cmd}");
            }
            if let Some(ref desc) = template.description {
                println!("    Description: {desc}");
            }
            println!("    Status: {status_str}");
            println!();
        } else {
            println!(
                "  {:<16} {:<10} {:<28} {}",
                format!("{}{origin}", template.name),
                engine,
                template.destination,
                status_str
            );
        }
    }

    println!(
        "\n{} templates ({} rendered, {} missing)",
        templates.len(),
        rendered_count,
        missing_count
    );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_add(
    profile: Option<String>,
    name: Option<String>,
    destination: Option<String>,
    engine: Option<String>,
    extension: Option<String>,
    render_command: Option<String>,
    description: Option<String>,
) -> Result<()> {
    let ctx = CliContext::load()?;
    let profile_name = ctx.resolve_profile(profile.as_deref());

    if !ctx.profile_exists(&profile_name) {
        print_error(&format!("Profile '{profile_name}' not found"));
        std::process::exit(1);
    }

    let existing = TemplateService::get_templates(&ctx.config.repo_path, &profile_name)?;

    let name = match name {
        Some(n) => n,
        None => prompt_string("Name (no extension)", None)?,
    };

    if existing.iter().any(|t| t.template.name == name) {
        print_error(&format!("Template '{name}' already exists in profile '{profile_name}'"));
        std::process::exit(1);
    }

    let destination = match destination {
        Some(d) => d,
        None => prompt_string("Destination (home-relative, e.g. .config/git/config)", None)?,
    };

    let dest_norm = destination.trim().trim_start_matches('/').to_string();
    if existing.iter().any(|t| t.template.destination == dest_norm) {
        print_error(&format!(
            "A template with destination '{dest_norm}' already exists in profile '{profile_name}'"
        ));
        std::process::exit(1);
    }

    let engine = match engine {
        Some(e) => parse_engine(&e).ok_or_else(|| anyhow::anyhow!("Invalid engine '{e}'. Valid: op-inject, custom"))?,
        None => prompt_engine()?,
    };

    let is_custom = engine == TemplateEngine::Custom;

    let extension = if is_custom {
        match extension {
            Some(x) => x,
            None => prompt_string("Source file extension (e.g. j2)", None)?,
        }
    } else {
        String::new()
    };

    let render_cmd = if is_custom {
        match render_command {
            Some(c) => c,
            None => prompt_string("Render command ({source}/{destination} supported)", None)?,
        }
    } else {
        String::new()
    };

    let desc = match description {
        Some(d) => d,
        None => prompt_string_optional("Description")?.unwrap_or_default(),
    };

    let params = TemplateCreationParams {
        name: &name,
        destination: &destination,
        engine: engine.clone(),
        extension: &extension,
        render_command: &render_cmd,
        description: &desc,
    };

    let validation = TemplateService::validate_template(&params);
    if !validation.is_valid {
        print_error(
            &validation
                .error_message
                .unwrap_or_else(|| "Validation failed".to_string()),
        );
        std::process::exit(1);
    }

    let template = TemplateService::create_template(params);
    let source_rel = template.source_relative_path(&profile_name);

    // Create the source stub so the user can author it before rendering.
    let created = TemplateService::ensure_source_stub(&ctx.config.repo_path, &profile_name, &template)?;
    TemplateService::add_template(&ctx.config.repo_path, &profile_name, template)?;

    print_success(&format!("Template '{name}' added to profile '{profile_name}'"));
    if created {
        print_info(&format!("Created source stub: {source_rel}"));
    } else {
        print_info(&format!("Using existing source: {source_rel}"));
    }

    Ok(())
}

fn cmd_remove(profile: Option<String>, yes: bool, name: Option<String>) -> Result<()> {
    let ctx = CliContext::load()?;
    let profile_name = ctx.resolve_profile(profile.as_deref());

    if !ctx.profile_exists(&profile_name) {
        print_error(&format!("Profile '{profile_name}' not found"));
        std::process::exit(1);
    }

    let templates = TemplateService::get_templates(&ctx.config.repo_path, &profile_name)?;

    if templates.is_empty() {
        println!("No templates found in profile '{profile_name}'");
        return Ok(());
    }

    // Only templates owned by this profile can be removed here.
    let owned: Vec<&_> = templates.iter().filter(|t| t.source_profile == profile_name).collect();
    if owned.is_empty() {
        println!(
            "No templates owned by profile '{profile_name}' (inherited templates must be removed from their parent)."
        );
        return Ok(());
    }

    let template_name = if let Some(n) = name {
        if owned.iter().any(|t| t.template.name == n) {
            n
        } else {
            print_error(&format!("Template '{n}' is not owned by profile '{profile_name}'"));
            std::process::exit(1);
        }
    } else {
        println!("Select template to remove from profile '{profile_name}':\n");
        let options: Vec<(String, Option<String>)> = owned
            .iter()
            .map(|t| {
                (
                    t.template.name.clone(),
                    Some(format!("({})", engine_str(&t.template.engine))),
                )
            })
            .collect();
        let options_ref: Vec<(&str, Option<&str>)> = options.iter().map(|(n, s)| (n.as_str(), s.as_deref())).collect();
        let selected = prompt_select_with_suffix("Template", &options_ref)?;
        owned[selected].template.name.clone()
    };

    if !yes {
        let confirm_msg = format!("Remove '{template_name}' from profile '{profile_name}'?");
        if !prompt_confirm(&confirm_msg)? {
            println!("Cancelled.");
            return Ok(());
        }
    }

    // Resolve the index within the profile's own (unsorted) templates list.
    let profile_templates = TemplateService::profile_templates(&ctx.config.repo_path, &profile_name)?;
    let index = profile_templates
        .iter()
        .position(|t| t.name == template_name)
        .ok_or_else(|| anyhow::anyhow!("Template '{template_name}' not found in profile"))?;

    TemplateService::delete_template(&ctx.config.repo_path, &profile_name, index)?;

    print_success(&format!(
        "Template '{template_name}' removed from profile '{profile_name}'"
    ));

    Ok(())
}

fn cmd_render(profile: Option<String>, _all: bool, name: Option<String>) -> Result<()> {
    let ctx = CliContext::load()?;
    let profile_name = ctx.resolve_profile(profile.as_deref());

    if !ctx.profile_exists(&profile_name) {
        print_error(&format!("Profile '{profile_name}' not found"));
        std::process::exit(1);
    }

    let templates = TemplateService::get_templates(&ctx.config.repo_path, &profile_name)?;

    if templates.is_empty() {
        println!("No templates configured for profile '{profile_name}'");
        return Ok(());
    }

    // Select which templates to render. A named template renders just that one;
    // otherwise (with or without --all) every template is rendered.
    let to_render: Vec<_> = if let Some(ref n) = name {
        let Some(t) = templates.iter().find(|t| t.template.name == *n) else {
            print_error(&format!("Template '{n}' not found in profile '{profile_name}'"));
            std::process::exit(1);
        };
        vec![t.clone()]
    } else {
        templates.clone()
    };

    let mut failures = 0;

    for resolved in &to_render {
        let template = &resolved.template;
        print_info(&format!(
            "Rendering '{}' ({}) -> {}",
            template.name,
            engine_str(&template.engine),
            template.destination
        ));

        match TemplateService::render_template(&ctx.config.repo_path, resolved) {
            Ok(outcome) => {
                for line in &outcome.output {
                    println!("    {line}");
                }
                if outcome.success {
                    print_success(&format!("Rendered '{}'", template.name));
                } else {
                    failures += 1;
                    print_error(&format!(
                        "Failed to render '{}': {}",
                        template.name,
                        outcome.error.unwrap_or_else(|| "unknown error".to_string())
                    ));
                }
            }
            Err(e) => {
                failures += 1;
                print_error(&format!("Failed to render '{}': {e}", template.name));
            }
        }
    }

    if failures > 0 {
        print_error(&format!(
            "{failures} of {} template(s) failed to render",
            to_render.len()
        ));
        std::process::exit(1);
    }

    print_success(&format!("Rendered {} template(s)", to_render.len()));
    Ok(())
}

fn cmd_check(profile: Option<String>) -> Result<()> {
    let ctx = CliContext::load()?;
    let profile_name = ctx.resolve_profile(profile.as_deref());

    if !ctx.profile_exists(&profile_name) {
        print_error(&format!("Profile '{profile_name}' not found"));
        std::process::exit(1);
    }

    let templates = TemplateService::get_templates(&ctx.config.repo_path, &profile_name)?;

    if templates.is_empty() {
        println!("No templates configured for profile '{profile_name}'");
        return Ok(());
    }

    let mut rendered_count = 0;
    let mut missing_count = 0;

    println!("Template status for profile '{profile_name}':\n");

    for resolved in &templates {
        let template = &resolved.template;
        let check = TemplateService::check_template(template);
        let status_str = match check.status {
            TemplateStatus::Rendered => {
                rendered_count += 1;
                "\u{2713} rendered".to_string()
            }
            TemplateStatus::Missing => {
                missing_count += 1;
                "\u{2717} missing".to_string()
            }
            TemplateStatus::Unknown => "? unknown".to_string(),
        };
        println!("  {:<16} {:<40} {}", template.name, check.destination, status_str);
    }

    println!(
        "\n{} templates ({} rendered, {} missing)",
        templates.len(),
        rendered_count,
        missing_count
    );

    Ok(())
}
