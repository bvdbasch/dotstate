//! Manage committed template definitions and render them without blocking the TUI.
//!
//! The source template file lives in the repo under the owning profile's
//! template subtree — `templates/<profile>/<destination>.<ext>` — mirroring how
//! synced files live under `<profile>/<path>`. The user only enters a
//! home-relative *destination*; the source path (and its extension) are derived
//! from the destination, engine, and owning profile. The extension is hidden
//! for built-in engines and only configurable for the `custom` engine, the same
//! way package fields stay hidden until the `custom` manager is chosen.

use crate::components::{footer::Footer, header::Header, FileBrowser, FileBrowserResult};
use crate::keymap::Action;
use crate::screens::{RenderContext, Screen, ScreenAction, ScreenContext};
use crate::services::{DiscoveredTemplate, TemplateCreationParams, TemplateService, TemplateStatus};
use crate::styles::theme;
use crate::ui::Screen as ScreenId;
use crate::utils::profile_manifest::{ResolvedTemplate, Template, TemplateEngine};
use crate::utils::{create_standard_layout, ProfileManifest, TextInput};
use crate::widgets::{TextInputWidget, TextInputWidgetExt};
use anyhow::{bail, Result};
use crossterm::event::{Event, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// Logical form fields. The engine row is a toggle; `Extension`/`Command` only
/// appear for the `custom` engine.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Destination,
    Engine,
    Extension,
    Command,
    Description,
}

struct Form {
    name: TextInput,
    destination: TextInput,
    extension: TextInput,
    command: TextInput,
    description: TextInput,
    engine: TemplateEngine,
    /// Index into `visible_fields()`.
    focus: usize,
    editing: Option<usize>,
    /// When set, an existing on-disk file to move into the template subtree on
    /// save (converting a flat file into a template without altering contents).
    import_file: Option<PathBuf>,
    /// When adopting a discovered source, its current repo-relative path. On
    /// save it is relocated to the canonical derived path if they differ.
    adopt_source: Option<String>,
    /// Preserve ownership inferred from a discovered source's path.
    adopt_profile: Option<String>,
}

impl Form {
    fn new(template: Option<&Template>) -> Self {
        Self {
            name: TextInput::with_text(template.map(|t| t.name.clone()).unwrap_or_default()),
            destination: TextInput::with_text(template.map(|t| t.destination.clone()).unwrap_or_default()),
            extension: TextInput::with_text(template.and_then(|t| t.extension.clone()).unwrap_or_default()),
            command: TextInput::with_text(template.and_then(|t| t.render_command.clone()).unwrap_or_default()),
            description: TextInput::with_text(template.and_then(|t| t.description.clone()).unwrap_or_default()),
            engine: template.map_or(TemplateEngine::OpInject, |t| t.engine.clone()),
            focus: 0,
            editing: None,
            import_file: None,
            adopt_source: None,
            adopt_profile: None,
        }
    }

    /// Build a new Add form pre-filled from a file chosen in the file browser.
    /// `relative_path` is home-relative (e.g. ".config/git/config"); `full_path`
    /// is the absolute on-disk location to move on save.
    fn from_file(full_path: PathBuf, relative_path: &str) -> Self {
        let mut form = Self::new(None);
        let dest = relative_path.trim_start_matches('/');
        form.destination.set_text(dest);
        // Derive a sensible name from the file stem (no extension).
        let name = std::path::Path::new(dest)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("template")
            .to_string();
        form.name.set_text(name);
        form.import_file = Some(full_path);
        form
    }

    /// Build an Add form pre-filled from a discovered (untracked) source file.
    /// The source already exists on disk, so no stub/import is needed on save —
    /// the user just confirms engine/command and the manifest entry is written.
    fn from_discovered(d: &DiscoveredTemplate) -> Self {
        let mut form = Self::new(None);
        form.name.set_text(d.name.clone());
        form.destination.set_text(d.destination.clone());
        form.engine = d.inferred_engine.clone();
        // For custom engines, carry the detected extension so the derived source
        // path matches the file already on disk.
        if d.inferred_engine == TemplateEngine::Custom {
            form.extension.set_text(d.extension.clone());
        }
        form.adopt_source = Some(d.source_relative.clone());
        form.adopt_profile = Some(d.source_profile.clone());
        form
    }

    fn is_custom(&self) -> bool {
        self.engine == TemplateEngine::Custom
    }

    /// The ordered fields currently visible for this engine.
    fn visible_fields(&self) -> Vec<Field> {
        let mut fields = vec![Field::Name, Field::Destination, Field::Engine];
        if self.is_custom() {
            fields.push(Field::Extension);
            fields.push(Field::Command);
        }
        fields.push(Field::Description);
        fields
    }

    fn focused_field(&self) -> Field {
        let fields = self.visible_fields();
        fields[self.focus.min(fields.len() - 1)]
    }

    fn input_mut(&mut self, field: Field) -> Option<&mut TextInput> {
        match field {
            Field::Name => Some(&mut self.name),
            Field::Destination => Some(&mut self.destination),
            Field::Extension => Some(&mut self.extension),
            Field::Command => Some(&mut self.command),
            Field::Description => Some(&mut self.description),
            Field::Engine => None,
        }
    }

    fn toggle_engine(&mut self) {
        self.engine = if self.is_custom() {
            TemplateEngine::OpInject
        } else {
            TemplateEngine::Custom
        };
        // Keep focus in range when field count shrinks.
        let len = self.visible_fields().len();
        if self.focus >= len {
            self.focus = len - 1;
        }
    }

    /// The source extension that would be applied, for preview text.
    fn preview_extension(&self) -> String {
        if self.is_custom() {
            self.extension.text().trim().trim_start_matches('.').to_string()
        } else {
            self.engine.default_extension().unwrap_or_default().to_string()
        }
    }

    /// Preview of the derived source path for the given owning profile.
    fn preview_source(&self, profile: &str) -> String {
        let dest = self.destination.text().trim().trim_start_matches('/');
        if dest.is_empty() {
            return "templates/<profile>/<destination>".to_string();
        }
        let ext = self.preview_extension();
        if ext.is_empty() {
            format!("templates/{profile}/{dest}")
        } else {
            format!("templates/{profile}/{dest}.{ext}")
        }
    }
}

#[derive(Default)]
pub struct ManageTemplatesScreen {
    templates: Vec<ResolvedTemplate>,
    /// Source files under `templates/` with no manifest entry yet.
    discovered: Vec<DiscoveredTemplate>,
    list: ListState,
    profile: String,
    form: Option<Form>,
    deleting: bool,
    message: String,
    worker: Option<Receiver<String>>,
    /// File browser for importing an existing on-disk file as a template source.
    file_browser: FileBrowser,
    list_area: Rect,
    /// Rendered input field rects mapped to their logical field.
    field_areas: Vec<(Rect, Field)>,
    engine_area: Rect,
    buttons: Vec<(Rect, Action)>,
    modal_buttons: Vec<(Rect, Action)>,
    last_click: Option<(usize, std::time::Instant)>,
}

/// A row in the unified list: either a tracked (manifest) template or a
/// discovered (untracked on-disk) source file awaiting adoption.
enum Row<'a> {
    Tracked(&'a ResolvedTemplate),
    Discovered(&'a DiscoveredTemplate),
}

impl ManageTemplatesScreen {
    pub fn new() -> Self {
        Self::default()
    }

    /// Total number of list rows (tracked templates followed by discovered).
    fn row_count(&self) -> usize {
        self.templates.len() + self.discovered.len()
    }

    /// Resolve a list index into either a tracked or discovered row.
    fn row_at(&self, index: usize) -> Option<Row<'_>> {
        if index < self.templates.len() {
            Some(Row::Tracked(&self.templates[index]))
        } else {
            self.discovered.get(index - self.templates.len()).map(Row::Discovered)
        }
    }

    fn selected_row(&self) -> Option<Row<'_>> {
        self.list.selected().and_then(|i| self.row_at(i))
    }

    fn selected_discovered(&self) -> Option<&DiscoveredTemplate> {
        match self.selected_row() {
            Some(Row::Discovered(d)) => Some(d),
            _ => None,
        }
    }

    fn reload(&mut self, ctx: &ScreenContext) -> Result<()> {
        let manifest = ProfileManifest::load_or_backfill(ctx.repo_path)?;
        self.templates = manifest.resolve_templates(ctx.active_profile)?;
        self.discovered = TemplateService::discover_untracked_templates(ctx.repo_path, ctx.active_profile)?;
        self.profile = ctx.active_profile.to_string();
        let total = self.row_count();
        let selection = self.list.selected().unwrap_or(0).min(total.saturating_sub(1));
        self.list.select((total > 0).then_some(selection));
        Ok(())
    }

    fn selected(&self) -> Option<&ResolvedTemplate> {
        match self.selected_row() {
            Some(Row::Tracked(t)) => Some(t),
            _ => None,
        }
    }

    /// Whether the selected template is owned by the active profile (editable
    /// here) versus inherited/common (edit the parent instead).
    fn selected_is_owned(&self) -> bool {
        self.selected().is_some_and(|t| t.source_profile == self.profile)
    }

    /// Index of the selected template within the active profile's own list.
    fn owned_index(&self, ctx: &ScreenContext) -> Result<Option<usize>> {
        let Some(selected) = self.selected() else {
            return Ok(None);
        };
        if selected.source_profile != self.profile {
            return Ok(None);
        }
        let own = TemplateService::profile_templates(ctx.repo_path, &self.profile)?;
        Ok(own.iter().position(|t| t.name == selected.template.name))
    }

    fn move_selection(&mut self, down: bool) {
        let total = self.row_count();
        if total == 0 {
            return;
        }
        let current = self.list.selected().unwrap_or(0);
        self.list.select(Some(if down {
            (current + 1).min(total - 1)
        } else {
            current.saturating_sub(1)
        }));
    }

    fn save(&mut self, ctx: &ScreenContext) -> Result<()> {
        let form = self.form.as_ref().expect("form open");
        let params = TemplateCreationParams {
            name: form.name.text(),
            destination: form.destination.text(),
            engine: form.engine.clone(),
            extension: form.extension.text(),
            render_command: form.command.text(),
            description: form.description.text(),
        };
        let validation = TemplateService::validate_template(&params);
        if !validation.is_valid {
            bail!("{}", validation.error_message.unwrap_or_default());
        }
        let template = TemplateService::create_template(params);

        if let Some(current_rel) = &form.adopt_source {
            let owner = form.adopt_profile.as_deref().unwrap_or(&self.profile);
            let source_rel = template.source_relative_path(owner);
            TemplateService::adopt_template(ctx.repo_path, current_rel, owner, template)?;
            self.message = format!("Added to '{owner}'; source {source_rel}");
            self.form = None;
            return self.reload(ctx);
        }

        // Guard against duplicate name/destination within the active profile's
        // own templates (ignoring the entry being edited).
        let own = TemplateService::profile_templates(ctx.repo_path, &self.profile)?;
        let editing = form.editing;
        if own
            .iter()
            .enumerate()
            .any(|(i, t)| Some(i) != editing && (t.name == template.name || t.destination == template.destination))
        {
            bail!("An owned template already uses that name or destination");
        }

        let import_file = form.import_file.clone();
        let source_rel = template.source_relative_path(&self.profile);

        // Materialize the source: import a flat file, adopt an existing
        // discovered file (relocating if needed), or create a stub to author.
        let message = if let Some(from) = import_file {
            TemplateService::import_source_file(ctx.repo_path, &self.profile, &template, &from)?;
            format!("Template saved; moved {} → {source_rel}", from.display())
        } else {
            let created = TemplateService::ensure_source_stub(ctx.repo_path, &self.profile, &template)?;
            if created {
                format!("Template saved; created source stub {source_rel}")
            } else {
                format!("Template saved; source {source_rel}")
            }
        };

        if let Some(index) = editing {
            TemplateService::update_template(ctx.repo_path, &self.profile, index, template)?;
        } else {
            TemplateService::add_template(ctx.repo_path, &self.profile, template)?;
        }
        self.form = None;
        self.message = message;
        self.reload(ctx)
    }

    fn start_render(&mut self, ctx: &ScreenContext, all: bool) {
        let templates: Vec<ResolvedTemplate> = if all {
            self.templates.clone()
        } else {
            self.selected().cloned().into_iter().collect()
        };
        if templates.is_empty() {
            return;
        }
        let repo = ctx.repo_path.to_path_buf();
        let (tx, rx) = mpsc::channel();
        self.worker = Some(rx);
        self.message = "Rendering templates…".to_string();
        std::thread::spawn(move || {
            let total = templates.len();
            let mut failures = Vec::new();
            for resolved in templates {
                let name = resolved.template.name.clone();
                let message = match TemplateService::render_template(&repo, &resolved) {
                    Ok(outcome)
                        if outcome.success
                            && TemplateService::check_template(&resolved.template).status
                                == TemplateStatus::Rendered =>
                    {
                        format!("Rendered '{name}'")
                    }
                    Ok(outcome) => {
                        let error = format!(
                            "{name}: {}",
                            outcome
                                .error
                                .unwrap_or_else(|| "Command succeeded but destination file is missing".to_string())
                        );
                        failures.push(error.clone());
                        error
                    }
                    Err(e) => {
                        let error = format!("{name}: {e}");
                        failures.push(error.clone());
                        error
                    }
                };
                if tx.send(message).is_err() {
                    break;
                }
            }
            let summary = if failures.is_empty() {
                format!("Rendered {total} template(s)")
            } else {
                format!("{} of {total} failed: {}", failures.len(), failures.join("; "))
            };
            let _ = tx.send(summary);
        });
    }

    /// Poll background rendering even while another screen is active.
    pub fn tick(&mut self) {
        if let Some(rx) = &self.worker {
            loop {
                match rx.try_recv() {
                    Ok(message) => self.message = message,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.worker = None;
                        break;
                    }
                }
            }
        }
    }

    fn open_edit_form(&mut self, ctx: &ScreenContext) -> Result<()> {
        // A discovered (untracked) row opens the adoption form instead.
        if let Some(discovered) = self.selected_discovered() {
            self.form = Some(Form::from_discovered(discovered));
            self.message = "Confirm the engine and details, then Save to add this file to the manifest".to_string();
            return Ok(());
        }
        match self.owned_index(ctx)? {
            Some(index) => {
                let template = self.selected().map(|r| r.template.clone());
                let mut form = Form::new(template.as_ref());
                form.editing = Some(index);
                self.form = Some(form);
            }
            None if self.selected().is_some() => {
                self.message =
                    "Inherited template: edit its parent profile, or Add an override with the same destination"
                        .to_string();
            }
            None => {}
        }
        Ok(())
    }

    fn action(&mut self, action: Action, ctx: &ScreenContext) -> Result<ScreenAction> {
        if self.form.is_some() {
            match action {
                Action::Cancel => {
                    self.form = None;
                    self.message.clear();
                }
                Action::Save | Action::Confirm => {
                    if let Err(e) = self.save(ctx) {
                        self.message = e.to_string();
                    }
                }
                Action::NextTab | Action::PrevTab => {
                    let form = self.form.as_mut().expect("form open");
                    let len = form.visible_fields().len();
                    form.focus = if action == Action::NextTab {
                        (form.focus + 1) % len
                    } else {
                        (form.focus + len - 1) % len
                    };
                }
                Action::MoveLeft | Action::MoveRight | Action::ToggleSelect
                    if self.form.as_ref().is_some_and(|f| f.focused_field() == Field::Engine) =>
                {
                    self.form.as_mut().expect("form open").toggle_engine();
                }
                _ => {}
            }
            return Ok(ScreenAction::None);
        }
        if self.deleting {
            match action {
                Action::Yes | Action::Confirm => {
                    if let Some(index) = self.owned_index(ctx)? {
                        TemplateService::delete_template(ctx.repo_path, &self.profile, index)?;
                        self.reload(ctx)?;
                        self.message = "Template removed; source and output files retained".to_string();
                    }
                    self.deleting = false;
                }
                Action::No | Action::Cancel => self.deleting = false,
                _ => {}
            }
            return Ok(ScreenAction::None);
        }
        match action {
            Action::MoveDown => self.move_selection(true),
            Action::MoveUp => self.move_selection(false),
            Action::Cancel => return Ok(ScreenAction::Navigate(ScreenId::MainMenu)),
            Action::Refresh | Action::CheckStatus => self.reload(ctx)?,
            Action::Create if self.worker.is_none() => self.form = Some(Form::new(None)),
            Action::Edit if self.worker.is_none() => self.open_edit_form(ctx)?,
            Action::Delete if self.worker.is_none() => {
                if self.selected_discovered().is_some() {
                    self.message = "Untracked file: press Enter to add it to the manifest first".to_string();
                } else if self.selected_is_owned() {
                    self.deleting = true;
                } else if self.selected().is_some() {
                    self.message = "Inherited templates must be removed from their parent profile".to_string();
                }
            }
            // Enter on a discovered row adopts it; otherwise it renders the
            // selected tracked template.
            Action::Confirm | Action::Install if self.worker.is_none() && self.selected_discovered().is_some() => {
                self.open_edit_form(ctx)?;
            }
            Action::Install | Action::Confirm if self.worker.is_none() => self.start_render(ctx, false),
            Action::SelectAll if self.worker.is_none() => self.start_render(ctx, true),
            Action::Import if self.worker.is_none() => {
                // Open the file browser to convert an existing on-disk file into
                // a template source.
                self.file_browser.open(crate::utils::get_home_dir());
            }
            _ => {}
        }
        Ok(ScreenAction::None)
    }

    fn render_buttons(
        frame: &mut Frame,
        area: Rect,
        actions: &[(Action, &str)],
        config: &crate::config::Config,
    ) -> Vec<(Rect, Action)> {
        let chunks = Layout::horizontal(vec![Constraint::Ratio(1, actions.len() as u32); actions.len()]).split(area);
        actions
            .iter()
            .zip(chunks.iter())
            .map(|((action, label), rect)| {
                let key = config.keymap.get_key_display_for_action(*action);
                frame.render_widget(
                    Paragraph::new(format!("{key}: {label}"))
                        .alignment(Alignment::Center)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(theme().border)),
                        ),
                    *rect,
                );
                (*rect, *action)
            })
            .collect()
    }

    fn field_label(field: Field) -> &'static str {
        match field {
            Field::Name => "Name (no extension)",
            Field::Destination => "Destination (home-relative, e.g. .config/git/config)",
            Field::Engine => "Engine (left/right or click to toggle)",
            Field::Extension => "Source extension (e.g. j2)",
            Field::Command => "Render command ({source}/{destination})",
            Field::Description => "Description",
        }
    }
}

impl Screen for ManageTemplatesScreen {
    fn on_enter(&mut self, ctx: &ScreenContext) -> Result<()> {
        self.form = None;
        self.deleting = false;
        self.file_browser.close();
        self.last_click = None;
        self.reload(ctx)
    }

    fn is_input_focused(&self) -> bool {
        self.form.is_some() || self.deleting || self.file_browser.is_input_focused()
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &RenderContext) -> Result<()> {
        let t = theme();
        frame.render_widget(Block::default().style(t.background_style()), area);
        let (header, body, footer) = create_standard_layout(area, 5, 3);
        Header::render(
            frame,
            header,
            "DotState - Manage Templates",
            "Committed sources → rendered destinations",
        )?;
        let rows = Layout::vertical([Constraint::Min(3), Constraint::Length(3), Constraint::Length(2)]).split(body);
        let panels = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).split(rows[0]);
        let block = Block::default()
            .title(format!(" Templates: {} ", self.profile))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(t.border));
        self.list_area = block.inner(panels[0]);
        let mut items: Vec<ListItem> = self
            .templates
            .iter()
            .map(|resolved| {
                let exists = TemplateService::check_template(&resolved.template).status == TemplateStatus::Rendered;
                let inherited = resolved.source_profile != self.profile;
                ListItem::new(format!(
                    "{} {}{}",
                    if exists { "✓" } else { "✗" },
                    resolved.template.name,
                    if inherited {
                        format!(" ({})", resolved.source_profile)
                    } else {
                        String::new()
                    }
                ))
            })
            .collect();
        // Discovered (untracked) rows render below, visually distinct.
        for d in &self.discovered {
            let origin = if d.source_profile == self.profile {
                String::new()
            } else {
                format!(" ({})", d.source_profile)
            };
            items.push(
                ListItem::new(format!("+ {}{}  — untracked", d.name, origin)).style(Style::default().fg(t.warning)),
            );
        }
        frame.render_stateful_widget(
            List::new(items)
                .block(block)
                .highlight_symbol(crate::styles::LIST_HIGHLIGHT_SYMBOL)
                .highlight_style(Style::default().bg(t.highlight_bg).fg(t.primary)),
            panels[0],
            &mut self.list,
        );
        let detail = match self.selected_row() {
            None => "No templates yet. Add a committed source, import a file, or drop a file under templates/ to discover it.".to_string(),
            Some(Row::Tracked(resolved)) => {
                let template = &resolved.template;
                let engine = if template.engine == TemplateEngine::OpInject {
                    "op-inject (1Password CLI)"
                } else {
                    "custom"
                };
                let origin = if resolved.source_profile == self.profile {
                    "this profile".to_string()
                } else {
                    format!("inherited from {}", resolved.source_profile)
                };
                format!(
                    "{}\n\nEngine: {engine}\nOrigin: {origin}\nSource: {}\nDestination: {}\n\n{}\n\n{}\n\nStatus checks file existence, not drift. Rendered files are not symlinked or synced.",
                    template.name,
                    resolved.source_relative_path(),
                    template.destination,
                    template.description.as_deref().unwrap_or(""),
                    template.render_command.as_deref().unwrap_or("")
                )
            }
            Some(Row::Discovered(d)) => {
                let engine = if d.inferred_engine == TemplateEngine::OpInject {
                    "op-inject (inferred from .tpl)"
                } else {
                    "custom (inferred from extension)"
                };
                format!(
                    "{}  — UNTRACKED\n\nThis source file exists under templates/ but is not in the manifest yet.\n\nSource: {}\nWould become profile: {}\nDerived destination: {}\nInferred engine: {engine}\n\nPress {} to add it to the manifest (confirm the engine and details first).",
                    d.name,
                    d.source_relative,
                    d.source_profile,
                    d.destination,
                    ctx.config.keymap.get_key_display_for_action(Action::Confirm),
                )
            }
        };
        frame.render_widget(
            Paragraph::new(detail).wrap(Wrap { trim: false }).block(
                Block::default()
                    .title(" Definition ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(t.border)),
            ),
            panels[1],
        );
        self.buttons = Self::render_buttons(
            frame,
            rows[1],
            &[
                (Action::Create, "Add"),
                (Action::Import, "From file"),
                (Action::Edit, "Edit"),
                (Action::Delete, "Remove"),
                (
                    Action::Install,
                    if self.selected_discovered().is_some() {
                        "Register"
                    } else {
                        "Render"
                    },
                ),
                (Action::SelectAll, "Render all"),
                (Action::Refresh, "Check"),
            ],
            ctx.config,
        );
        frame.render_widget(
            Paragraph::new(self.message.as_str()).style(Style::default().fg(t.text)),
            rows[2],
        );
        Footer::render(
            frame,
            footer,
            &format!(
                "{}: Navigate | {}: Register / render | {}: Back{}",
                ctx.config.keymap.navigation_display(),
                ctx.config.keymap.get_key_display_for_action(Action::Confirm),
                ctx.config.keymap.get_key_display_for_action(Action::Cancel),
                if self.worker.is_some() { " | Rendering…" } else { "" }
            ),
        )?;
        self.field_areas.clear();
        self.modal_buttons.clear();
        if let Some(form) = &self.form {
            let popup = crate::utils::center_popup(area, 85, 100);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .title(if form.editing.is_some() {
                    " Edit template "
                } else {
                    " Add template "
                })
                .borders(Borders::ALL)
                .style(t.background_style());
            let inner = block.inner(popup);
            frame.render_widget(block, popup);
            // Reserve rows: fields area, source preview (2), message (2), buttons (3).
            let sections = Layout::vertical([
                Constraint::Min(3),
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Length(3),
            ])
            .split(inner);

            let visible = form.visible_fields();
            let per_row = 3u16;
            let capacity = (sections[0].height / per_row).clamp(1, visible.len() as u16) as usize;
            let start = form.focus.saturating_sub(capacity.saturating_sub(1));
            let end = (start + capacity).min(visible.len());
            let field_rows = Layout::vertical(vec![Constraint::Length(per_row); end - start]).split(sections[0]);
            self.engine_area = Rect::default();
            for (row, idx) in (start..end).enumerate() {
                let field = visible[idx];
                let focused = form.focus == idx;
                let rect = field_rows[row];
                if field == Field::Engine {
                    self.engine_area = rect;
                    let engine = if form.is_custom() { "custom" } else { "op-inject" };
                    frame.render_widget(
                        Paragraph::new(format!("Engine: {engine}")).block(
                            Block::default()
                                .title(Self::field_label(Field::Engine))
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(if focused { t.primary } else { t.border })),
                        ),
                        rect,
                    );
                } else if let Some(input) = match field {
                    Field::Name => Some(&form.name),
                    Field::Destination => Some(&form.destination),
                    Field::Extension => Some(&form.extension),
                    Field::Command => Some(&form.command),
                    Field::Description => Some(&form.description),
                    Field::Engine => None,
                } {
                    frame.render_text_input_widget(
                        TextInputWidget::new(input)
                            .title(Self::field_label(field))
                            .focused(focused),
                        rect,
                    );
                    self.field_areas.push((rect, field));
                }
            }

            // Derived source preview, so there's zero ambiguity about placement.
            frame.render_widget(
                Paragraph::new(format!(
                    "Owner: {} | Source → {}",
                    form.adopt_profile.as_deref().unwrap_or(&self.profile),
                    form.preview_source(form.adopt_profile.as_deref().unwrap_or(&self.profile))
                ))
                .style(Style::default().fg(t.text_muted))
                .wrap(Wrap { trim: true }),
                sections[1],
            );
            frame.render_widget(
                Paragraph::new(self.message.as_str()).wrap(Wrap { trim: true }),
                sections[2],
            );
            self.modal_buttons = Self::render_buttons(
                frame,
                sections[3],
                &[(Action::Save, "Save"), (Action::Cancel, "Cancel")],
                ctx.config,
            );
        } else if self.deleting {
            let popup = crate::utils::center_popup(area, 60, 30);
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .title(" Remove template? ")
                .borders(Borders::ALL)
                .style(t.background_style());
            let inner = block.inner(popup);
            frame.render_widget(block, popup);
            let sections = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).split(inner);
            frame.render_widget(
                Paragraph::new(
                    "Remove the definition from this profile? The source and rendered output files are retained.",
                )
                .wrap(Wrap { trim: true }),
                sections[0],
            );
            self.modal_buttons = Self::render_buttons(
                frame,
                sections[1],
                &[(Action::Yes, "Remove"), (Action::No, "Cancel")],
                ctx.config,
            );
        }

        // File browser overlays everything when importing an on-disk file.
        if self.file_browser.is_open() {
            self.file_browser
                .render(frame, area, ctx.config, ctx.syntax_set, ctx.syntax_theme)?;
        }
        Ok(())
    }

    fn handle_event(&mut self, event: Event, ctx: &ScreenContext) -> Result<ScreenAction> {
        // File browser (import flow) takes precedence while open.
        if self.file_browser.is_open() {
            match self.file_browser.handle_event(event, ctx.config)? {
                FileBrowserResult::None | FileBrowserResult::RefreshNeeded => return Ok(ScreenAction::None),
                FileBrowserResult::Cancelled => {
                    self.message = "Import cancelled".to_string();
                    return Ok(ScreenAction::None);
                }
                FileBrowserResult::Selected {
                    full_path,
                    relative_path,
                } => {
                    // Pre-fill an Add form from the chosen file; the user picks
                    // the engine, and the file is moved in on save.
                    self.form = Some(Form::from_file(full_path, &relative_path));
                    self.message = "Pick an engine, then Save to convert this file into a template".to_string();
                    return Ok(ScreenAction::None);
                }
            }
        }
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let action = ctx.config.keymap.get_action(key.code, key.modifiers);
                // Text entry takes priority over printable navigation shortcuts (including q).
                if let Some(form) = self.form.as_mut() {
                    let field = form.focused_field();
                    let is_text_field = field != Field::Engine;
                    if is_text_field
                        && !matches!(
                            action,
                            Some(Action::Save | Action::Confirm | Action::Cancel | Action::NextTab | Action::PrevTab)
                        )
                    {
                        if let Some(input) = form.input_mut(field) {
                            input.handle_key(key.code);
                        }
                        return Ok(ScreenAction::None);
                    }
                }
                if let Some(action) = action {
                    return self.action(action, ctx);
                }
            }
            Event::Mouse(mouse) => {
                let point = Position::new(mouse.column, mouse.row);
                if self.form.is_some() || self.deleting {
                    if let Some(form) = self.form.as_mut() {
                        let len = form.visible_fields().len();
                        match mouse.kind {
                            MouseEventKind::ScrollDown => form.focus = (form.focus + 1).min(len - 1),
                            MouseEventKind::ScrollUp => form.focus = form.focus.saturating_sub(1),
                            _ => {}
                        }
                    }
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                        if let Some((_, action)) = self.modal_buttons.iter().find(|(rect, _)| rect.contains(point)) {
                            return self.action(*action, ctx);
                        }
                        if let Some(form) = self.form.as_mut() {
                            let visible = form.visible_fields();
                            if let Some((_, field)) = self.field_areas.iter().find(|(rect, _)| rect.contains(point)) {
                                if let Some(i) = visible.iter().position(|f| f == field) {
                                    form.focus = i;
                                }
                            }
                            if self.engine_area.contains(point) {
                                if let Some(i) = visible.iter().position(|f| *f == Field::Engine) {
                                    form.focus = i;
                                }
                                form.toggle_engine();
                            }
                        }
                    }
                    return Ok(ScreenAction::None);
                }
                match mouse.kind {
                    MouseEventKind::ScrollDown if self.list_area.contains(point) => self.move_selection(true),
                    MouseEventKind::ScrollUp if self.list_area.contains(point) => self.move_selection(false),
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some((_, action)) = self.buttons.iter().find(|(rect, _)| rect.contains(point)) {
                            return self.action(*action, ctx);
                        }
                        if self.list_area.contains(point) {
                            let index = self.list.offset() + usize::from(mouse.row - self.list_area.y);
                            if index < self.row_count() {
                                let now = std::time::Instant::now();
                                let double_click = self.last_click.is_some_and(|(previous, time)| {
                                    previous == index
                                        && now.duration_since(time) < std::time::Duration::from_millis(400)
                                });
                                self.list.select(Some(index));
                                self.last_click = Some((index, now));
                                if double_click && self.selected_discovered().is_some() {
                                    self.last_click = None;
                                    return self.action(Action::Confirm, ctx);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(ScreenAction::None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
    use ratatui::{backend::TestBackend, Terminal};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    fn setup() -> (TempDir, Config) {
        let dir = TempDir::new().unwrap();
        let config = Config {
            repo_path: dir.path().to_path_buf(),
            active_profile: "child".to_string(),
            ..Config::default()
        };
        let mut manifest = ProfileManifest::default();
        manifest.add_profile("parent".to_string(), None);
        manifest.add_profile_with_inherits("child".to_string(), None, Some("parent".to_string()));
        manifest.save(dir.path()).unwrap();
        (dir, config)
    }

    fn custom_template(name: &str, dest: &str) -> Template {
        Template {
            name: name.to_string(),
            destination: dest.to_string(),
            engine: TemplateEngine::Custom,
            extension: Some("in".to_string()),
            render_command: Some("cp '{source}' '{destination}'".to_string()),
            description: None,
        }
    }

    fn draw(screen: &mut ManageTemplatesScreen, config: &Config, width: u16, height: u16) {
        let syntaxes = syntect::parsing::SyntaxSet::load_defaults_newlines();
        let themes = syntect::highlighting::ThemeSet::load_defaults();
        let theme = themes.themes.values().next().unwrap();
        let context = RenderContext::new(config, &syntaxes, &themes, theme);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area(), &context).unwrap())
            .unwrap();
    }

    #[test]
    fn editing_and_deleting_use_owned_manifest_index_not_sorted_or_inherited_index() {
        let (dir, config) = setup();
        TemplateService::add_template(dir.path(), "parent", custom_template("aaa_inherited", "a/out")).unwrap();
        TemplateService::add_template(dir.path(), "child", custom_template("zzz_first", "z/out")).unwrap();
        TemplateService::add_template(dir.path(), "child", custom_template("bbb_second", "b/out")).unwrap();
        let config_path = dir.path().join("config.toml");
        let ctx = ScreenContext::new(&config, &config_path);
        let mut screen = ManageTemplatesScreen::new();
        screen.on_enter(&ctx).unwrap();

        // Resolved list is sorted: aaa_inherited(parent), bbb_second, zzz_first.
        // Select bbb_second (owned) and edit its name.
        let idx = screen
            .templates
            .iter()
            .position(|t| t.template.name == "bbb_second")
            .unwrap();
        screen.list.select(Some(idx));
        screen.action(Action::Edit, &ctx).unwrap();
        screen.form.as_mut().unwrap().name.set_text("edited_second");
        screen.action(Action::Save, &ctx).unwrap();
        let manifest = ProfileManifest::load(dir.path()).unwrap();
        let child = manifest.profiles.iter().find(|p| p.name == "child").unwrap();
        assert_eq!(child.templates[0].name, "zzz_first");
        assert_eq!(child.templates[1].name, "edited_second");

        // Deleting an inherited template is blocked.
        let inherited_idx = screen
            .templates
            .iter()
            .position(|t| t.source_profile == "parent")
            .unwrap();
        screen.list.select(Some(inherited_idx));
        screen.action(Action::Delete, &ctx).unwrap();
        assert!(!screen.deleting);
    }

    #[test]
    fn add_form_derives_source_from_destination_and_engine_extension() {
        let (dir, config) = setup();
        let config_path = dir.path().join("config.toml");
        let ctx = ScreenContext::new(&config, &config_path);
        let mut screen = ManageTemplatesScreen::new();
        screen.on_enter(&ctx).unwrap();
        screen.action(Action::Create, &ctx).unwrap();
        let form = screen.form.as_mut().unwrap();
        form.name.set_text("work");
        form.destination.set_text(".config/git/work");
        // Default engine is op-inject → extension tpl, hidden field.
        assert_eq!(form.preview_source("child"), "templates/child/.config/git/work.tpl");
        screen.action(Action::Save, &ctx).unwrap();
        assert!(screen.form.is_none());

        let manifest = ProfileManifest::load(dir.path()).unwrap();
        let child = manifest.profiles.iter().find(|p| p.name == "child").unwrap();
        let saved = &child.templates[0];
        assert_eq!(saved.destination, ".config/git/work");
        assert_eq!(saved.engine, TemplateEngine::OpInject);
        assert!(saved.extension.is_none());
        // Stub source file created at the derived path.
        let stub = dir.path().join("templates/child/.config/git/work.tpl");
        assert!(stub.exists(), "expected stub at {}", stub.display());
    }

    #[test]
    fn custom_engine_reveals_extension_and_command_fields() {
        let (dir, config) = setup();
        let config_path = dir.path().join("config.toml");
        let ctx = ScreenContext::new(&config, &config_path);
        let mut screen = ManageTemplatesScreen::new();
        screen.on_enter(&ctx).unwrap();
        screen.action(Action::Create, &ctx).unwrap();
        let form = screen.form.as_ref().unwrap();
        // op-inject hides extension + command: Name, Destination, Engine, Description.
        assert_eq!(form.visible_fields().len(), 4);
        // Focus the engine row and toggle to custom.
        let engine_pos = form.visible_fields().iter().position(|f| *f == Field::Engine).unwrap();
        screen.form.as_mut().unwrap().focus = engine_pos;
        screen.action(Action::MoveRight, &ctx).unwrap();
        let form = screen.form.as_ref().unwrap();
        assert!(form.is_custom());
        // Now 6 fields including Extension + Command.
        assert_eq!(form.visible_fields().len(), 6);
    }

    #[test]
    fn text_shortcuts_do_not_activate_background_actions_in_form() {
        let (dir, config) = setup();
        let config_path = dir.path().join("config.toml");
        let ctx = ScreenContext::new(&config, &config_path);
        let mut screen = ManageTemplatesScreen::new();
        screen.on_enter(&ctx).unwrap();
        screen.action(Action::Create, &ctx).unwrap();
        for ch in "qjed".chars() {
            screen
                .handle_event(Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)), &ctx)
                .unwrap();
        }
        assert_eq!(screen.form.as_ref().unwrap().name.text(), "qjed");
        // Modal cancel button closes the form, not navigation.
        draw(&mut screen, &config, 100, 32);
        let cancel = screen.modal_buttons[1].0;
        screen
            .handle_event(
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: cancel.x + 1,
                    row: cancel.y + 1,
                    modifiers: KeyModifiers::NONE,
                }),
                &ctx,
            )
            .unwrap();
        assert!(screen.form.is_none());
    }

    #[test]
    fn background_render_resolves_source_under_profile_subtree() {
        let (dir, config) = setup();
        // Absolute destination inside the temp dir keeps the test hermetic
        // (no HOME expansion, no parallel-env hazard).
        let dest = dir.path().join("rendered.out");
        let tpl = custom_template("rendered", dest.to_str().unwrap());
        // Author the source stub with known content under the owning profile subtree.
        let source = dir.path().join(tpl.source_relative_path("child"));
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "private fixture\n").unwrap();
        TemplateService::add_template(dir.path(), "child", tpl).unwrap();

        let config_path = dir.path().join("config.toml");
        let ctx = ScreenContext::new(&config, &config_path);
        let mut screen = ManageTemplatesScreen::new();
        screen.on_enter(&ctx).unwrap();
        screen.list.select(Some(0));
        screen.action(Action::Install, &ctx).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while screen.worker.is_some() {
            assert!(Instant::now() < deadline, "render worker timed out");
            std::thread::sleep(Duration::from_millis(10));
            screen.tick();
        }
        assert_eq!(screen.message, "Rendered 1 template(s)");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "private fixture\n");
        assert!(!std::fs::symlink_metadata(&dest).unwrap().file_type().is_symlink());
        draw(&mut screen, &config, 80, 24);
    }

    #[test]
    fn importing_a_file_prefills_the_form_and_moves_it_into_the_profile_subtree() {
        let (dir, config) = setup();
        let config_path = dir.path().join("config.toml");
        let ctx = ScreenContext::new(&config, &config_path);
        let mut screen = ManageTemplatesScreen::new();
        screen.on_enter(&ctx).unwrap();

        // Simulate the file-browser selection step directly: pre-fill a form
        // from an existing on-disk file with a home-relative destination.
        let on_disk = dir.path().join("flat.conf");
        std::fs::write(&on_disk, "secret = REPLACE_ME\n").unwrap();
        screen.form = Some(Form::from_file(on_disk.clone(), ".config/newtool/config"));

        // Name is derived from the file stem, destination carried over.
        let form = screen.form.as_ref().unwrap();
        assert_eq!(form.name.text(), "config");
        assert_eq!(form.destination.text(), ".config/newtool/config");
        assert!(form.import_file.is_some());

        // Keep the default op-inject engine and save.
        screen.action(Action::Save, &ctx).unwrap();
        assert!(screen.form.is_none());

        // The on-disk file was moved into the child profile's template subtree
        // with the engine extension, contents preserved; original is gone.
        assert!(!on_disk.exists(), "original flat file should be moved");
        let moved = dir.path().join("templates/child/.config/newtool/config.tpl");
        assert_eq!(std::fs::read_to_string(&moved).unwrap(), "secret = REPLACE_ME\n");

        // The manifest records the template for the active profile.
        let manifest = ProfileManifest::load(dir.path()).unwrap();
        let child = manifest.profiles.iter().find(|p| p.name == "child").unwrap();
        assert_eq!(child.templates.len(), 1);
        assert_eq!(child.templates[0].destination, ".config/newtool/config");
    }

    #[test]
    fn untracked_file_is_discovered_and_can_be_adopted_into_the_manifest() {
        let (dir, config) = setup();
        // Hand-place a source file under the active profile's subtree with no
        // manifest entry (Scenario 1: drop a .tpl in the right folder).
        let src = dir.path().join("templates/child/.config/git/work.tpl");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, "email = {{ op://v/i/email }}\n").unwrap();

        let config_path = dir.path().join("config.toml");
        let ctx = ScreenContext::new(&config, &config_path);
        let mut screen = ManageTemplatesScreen::new();
        screen.on_enter(&ctx).unwrap();

        // The discovered row shows up (no tracked templates, one discovered).
        assert_eq!(screen.templates.len(), 0);
        assert_eq!(screen.discovered.len(), 1);
        assert_eq!(screen.row_count(), 1);

        // Select it and press Enter → adoption form opens, pre-filled.
        screen.list.select(Some(0));
        assert!(screen.selected_discovered().is_some());
        screen.action(Action::Confirm, &ctx).unwrap();
        let form = screen.form.as_ref().expect("adoption form open");
        assert_eq!(form.name.text(), "work");
        assert_eq!(form.destination.text(), ".config/git/work");
        assert_eq!(form.engine, TemplateEngine::OpInject);

        // Save adopts it: manifest entry written, existing file kept (not re-stubbed).
        screen.action(Action::Save, &ctx).unwrap();
        assert!(screen.form.is_none());
        assert_eq!(std::fs::read_to_string(&src).unwrap(), "email = {{ op://v/i/email }}\n");

        let manifest = ProfileManifest::load(dir.path()).unwrap();
        let child = manifest.profiles.iter().find(|p| p.name == "child").unwrap();
        assert_eq!(child.templates.len(), 1);
        assert_eq!(child.templates[0].destination, ".config/git/work");

        // After adoption it is tracked, no longer discovered.
        screen.on_enter(&ctx).unwrap();
        assert_eq!(screen.templates.len(), 1);
        assert_eq!(screen.discovered.len(), 0);
    }

    #[test]
    fn root_level_untracked_file_is_attributed_to_active_profile() {
        let (dir, config) = setup();
        // File directly under templates/ with no profile segment.
        let src = dir.path().join("templates/.config/app.tpl");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, "x\n").unwrap();

        let config_path = dir.path().join("config.toml");
        let ctx = ScreenContext::new(&config, &config_path);
        let mut screen = ManageTemplatesScreen::new();
        screen.on_enter(&ctx).unwrap();

        assert_eq!(screen.discovered.len(), 1);
        assert_eq!(screen.discovered[0].source_profile, "child"); // active profile
        assert_eq!(screen.discovered[0].destination, ".config/app");

        // Adopting it relocates the root-level file into the profile subtree.
        screen.list.select(Some(0));
        screen.action(Action::Confirm, &ctx).unwrap();
        screen.action(Action::Save, &ctx).unwrap();
        assert!(!src.exists(), "root-level source should be relocated");
        let moved = dir.path().join("templates/child/.config/app.tpl");
        assert_eq!(std::fs::read_to_string(&moved).unwrap(), "x\n");
    }

    #[test]
    fn discovery_adopts_common_and_other_profile_without_reassigning_to_active() {
        let (dir, config) = setup();
        for owner in ["common", "parent"] {
            let source = dir.path().join(format!("templates/{owner}/.config/{owner}.tpl"));
            std::fs::create_dir_all(source.parent().unwrap()).unwrap();
            std::fs::write(source, "fixture\n").unwrap();
        }
        let config_path = dir.path().join("config.toml");
        let ctx = ScreenContext::new(&config, &config_path);
        let mut screen = ManageTemplatesScreen::new();
        screen.on_enter(&ctx).unwrap();
        for owner in ["common", "parent"] {
            let index = screen.templates.len()
                + screen
                    .discovered
                    .iter()
                    .position(|d| d.source_profile == owner)
                    .unwrap();
            screen.list.select(Some(index));
            screen.action(Action::Confirm, &ctx).unwrap();
            screen.action(Action::Save, &ctx).unwrap();
            assert!(screen.form.is_none());
            assert_eq!(
                std::fs::read_to_string(dir.path().join(format!("templates/{owner}/.config/{owner}.tpl"))).unwrap(),
                "fixture\n"
            );
        }
        let manifest = ProfileManifest::load(dir.path()).unwrap();
        assert_eq!(manifest.common.templates.len(), 1);
        assert_eq!(
            manifest
                .profiles
                .iter()
                .find(|p| p.name == "parent")
                .unwrap()
                .templates
                .len(),
            1
        );
        assert_eq!(
            manifest
                .profiles
                .iter()
                .find(|p| p.name == "child")
                .unwrap()
                .templates
                .len(),
            0
        );
        assert_eq!(screen.discovered.len(), 0);
    }

    #[test]
    fn single_click_selects_and_double_click_opens_discovered_registration() {
        let (dir, config) = setup();
        let source = dir.path().join("templates/root.tpl");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(source, "fixture").unwrap();
        let config_path = dir.path().join("config.toml");
        let ctx = ScreenContext::new(&config, &config_path);
        let mut screen = ManageTemplatesScreen::new();
        screen.on_enter(&ctx).unwrap();
        draw(&mut screen, &config, 100, 32);
        let click = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: screen.list_area.x + 1,
            row: screen.list_area.y,
            modifiers: KeyModifiers::NONE,
        });
        screen.handle_event(click.clone(), &ctx).unwrap();
        assert!(screen.form.is_none());
        screen.handle_event(click, &ctx).unwrap();
        assert!(screen.form.is_some());
        screen.action(Action::Cancel, &ctx).unwrap();
        assert_eq!(
            ProfileManifest::load(dir.path())
                .unwrap()
                .profiles
                .iter()
                .map(|p| p.templates.len())
                .sum::<usize>(),
            0
        );
    }
}
