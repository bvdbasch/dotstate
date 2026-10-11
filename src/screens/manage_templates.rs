//! Manage committed template definitions and render them without blocking the TUI.
//!
//! The source template file lives in the repo under the owning profile's
//! template subtree — `templates/<profile>/<destination>.<ext>` — mirroring how
//! synced files live under `<profile>/<path>`. The user only enters a
//! home-relative *destination*; the source path (and its extension) are derived
//! from the destination, engine, and owning profile. The extension is hidden
//! for built-in engines and only configurable for the `custom` engine, the same
//! way package fields stay hidden until the `custom` manager is chosen.

use crate::components::{footer::Footer, header::Header};
use crate::keymap::Action;
use crate::screens::{RenderContext, Screen, ScreenAction, ScreenContext};
use crate::services::{TemplateCreationParams, TemplateService, TemplateStatus};
use crate::styles::theme;
use crate::ui::Screen as ScreenId;
use crate::utils::profile_manifest::{ResolvedTemplate, Template, TemplateEngine};
use crate::utils::{create_standard_layout, ProfileManifest, TextInput};
use crate::widgets::{TextInputWidget, TextInputWidgetExt};
use anyhow::{bail, Result};
use crossterm::event::{Event, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
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
        }
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
    list: ListState,
    profile: String,
    form: Option<Form>,
    deleting: bool,
    message: String,
    worker: Option<Receiver<String>>,
    list_area: Rect,
    /// Rendered input field rects mapped to their logical field.
    field_areas: Vec<(Rect, Field)>,
    engine_area: Rect,
    buttons: Vec<(Rect, Action)>,
    modal_buttons: Vec<(Rect, Action)>,
}

impl ManageTemplatesScreen {
    pub fn new() -> Self {
        Self::default()
    }

    fn reload(&mut self, ctx: &ScreenContext) -> Result<()> {
        let manifest = ProfileManifest::load_or_backfill(ctx.repo_path)?;
        self.templates = manifest.resolve_templates(ctx.active_profile)?;
        self.profile = ctx.active_profile.to_string();
        let selection = self
            .list
            .selected()
            .unwrap_or(0)
            .min(self.templates.len().saturating_sub(1));
        self.list.select((!self.templates.is_empty()).then_some(selection));
        Ok(())
    }

    fn selected(&self) -> Option<&ResolvedTemplate> {
        self.list.selected().and_then(|i| self.templates.get(i))
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
        if self.templates.is_empty() {
            return;
        }
        let current = self.list.selected().unwrap_or(0);
        self.list.select(Some(if down {
            (current + 1).min(self.templates.len() - 1)
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

        let created = TemplateService::ensure_source_stub(ctx.repo_path, &self.profile, &template)?;
        let source_rel = template.source_relative_path(&self.profile);

        if let Some(index) = editing {
            TemplateService::update_template(ctx.repo_path, &self.profile, index, template)?;
        } else {
            TemplateService::add_template(ctx.repo_path, &self.profile, template)?;
        }
        self.form = None;
        self.message = if created {
            format!("Template saved; created source stub {source_rel}")
        } else {
            format!("Template saved; source {source_rel}")
        };
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
                if self.selected_is_owned() {
                    self.deleting = true;
                } else if self.selected().is_some() {
                    self.message = "Inherited templates must be removed from their parent profile".to_string();
                }
            }
            Action::Install | Action::Confirm if self.worker.is_none() => self.start_render(ctx, false),
            Action::SelectAll if self.worker.is_none() => self.start_render(ctx, true),
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
        self.reload(ctx)
    }

    fn is_input_focused(&self) -> bool {
        self.form.is_some() || self.deleting
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
        let items: Vec<_> = self
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
        frame.render_stateful_widget(
            List::new(items)
                .block(block)
                .highlight_symbol(crate::styles::LIST_HIGHLIGHT_SYMBOL)
                .highlight_style(Style::default().bg(t.highlight_bg).fg(t.primary)),
            panels[0],
            &mut self.list,
        );
        let detail = self.selected().map_or_else(
            || "No templates yet. Add a committed source and choose its destination.".to_string(),
            |resolved| {
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
            },
        );
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
                (Action::Edit, "Edit"),
                (Action::Delete, "Remove"),
                (Action::Install, "Render"),
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
                "{}: Navigate | {}: Back{}",
                ctx.config.keymap.navigation_display(),
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
                Paragraph::new(format!("Source → {}", form.preview_source(&self.profile)))
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
        Ok(())
    }

    fn handle_event(&mut self, event: Event, ctx: &ScreenContext) -> Result<ScreenAction> {
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
                            if index < self.templates.len() {
                                self.list.select(Some(index));
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
}
