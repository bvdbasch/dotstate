//! Manage committed template definitions and render them without blocking the TUI.

use crate::components::{footer::Footer, header::Header};
use crate::keymap::Action;
use crate::screens::{RenderContext, Screen, ScreenAction, ScreenContext};
use crate::services::{TemplateCreationParams, TemplateService, TemplateStatus};
use crate::styles::theme;
use crate::ui::Screen as ScreenId;
use crate::utils::profile_manifest::{Template, TemplateEngine};
use crate::utils::{create_standard_layout, ProfileManifest, TextInput};
use crate::widgets::{TextInputWidget, TextInputWidgetExt};
use anyhow::{bail, Result};
use crossterm::event::{Event, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use std::sync::mpsc::{self, Receiver, TryRecvError};

const LABELS: [&str; 5] = [
    "Name",
    "Source (repo-relative)",
    "Destination",
    "Custom command ({source}/{destination})",
    "Description",
];

struct Form {
    fields: [TextInput; 5],
    engine: TemplateEngine,
    focus: usize,
    editing: Option<usize>,
}

impl Form {
    fn new(template: Option<&Template>, editing: Option<usize>) -> Self {
        let values = template.map_or_else(
            || {
                [
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                ]
            },
            |t| {
                [
                    t.name.clone(),
                    t.source.clone(),
                    t.destination.clone(),
                    t.render_command.clone().unwrap_or_default(),
                    t.description.clone().unwrap_or_default(),
                ]
            },
        );
        Self {
            fields: values.map(TextInput::with_text),
            engine: template.map_or(TemplateEngine::OpInject, |t| t.engine.clone()),
            focus: 0,
            editing,
        }
    }
}

#[derive(Default)]
pub struct ManageTemplatesScreen {
    templates: Vec<Template>,
    owned: Vec<Template>,
    list: ListState,
    profile: String,
    form: Option<Form>,
    deleting: bool,
    message: String,
    worker: Option<Receiver<String>>,
    list_area: Rect,
    field_areas: Vec<(Rect, usize)>,
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
        self.owned = manifest
            .profiles
            .iter()
            .find(|p| p.name == ctx.active_profile)
            .map(|p| p.templates.clone())
            .unwrap_or_default();
        self.profile = ctx.active_profile.to_string();
        let selection = self
            .list
            .selected()
            .unwrap_or(0)
            .min(self.templates.len().saturating_sub(1));
        self.list.select((!self.templates.is_empty()).then_some(selection));
        Ok(())
    }

    fn selected(&self) -> Option<&Template> {
        self.list.selected().and_then(|i| self.templates.get(i))
    }

    fn owned_index(&self) -> Option<usize> {
        self.selected()
            .and_then(|t| self.owned.iter().position(|own| own.destination == t.destination))
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
            name: form.fields[0].text(),
            source: form.fields[1].text(),
            destination: form.fields[2].text(),
            engine: form.engine.clone(),
            render_command: form.fields[3].text(),
            description: form.fields[4].text(),
        };
        let validation = TemplateService::validate_template(&params);
        if !validation.is_valid {
            bail!("{}", validation.error_message.unwrap_or_default());
        }
        let template = TemplateService::create_template(params);
        if self
            .owned
            .iter()
            .enumerate()
            .any(|(i, t)| Some(i) != form.editing && (t.name == template.name || t.destination == template.destination))
        {
            bail!("An owned template already uses that name or destination");
        }
        if let Some(index) = form.editing {
            TemplateService::update_template(ctx.repo_path, ctx.active_profile, index, template)?;
        } else {
            TemplateService::add_template(ctx.repo_path, ctx.active_profile, template)?;
        }
        self.form = None;
        self.message = "Template saved".to_string();
        self.reload(ctx)
    }

    fn start_render(&mut self, ctx: &ScreenContext, all: bool) {
        let templates = if all {
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
            for template in templates {
                let message = match TemplateService::render_template(&repo, &template) {
                    Ok(outcome)
                        if outcome.success
                            && TemplateService::check_template(&template).status == TemplateStatus::Rendered =>
                    {
                        format!("Rendered '{}'", template.name)
                    }
                    Ok(outcome) => {
                        let error = format!(
                            "{}: {}",
                            template.name,
                            outcome
                                .error
                                .unwrap_or_else(|| "Command succeeded but destination file is missing".to_string())
                        );
                        failures.push(error.clone());
                        error
                    }
                    Err(e) => {
                        let error = format!("{}: {e}", template.name);
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
                    form.focus = (form.focus + if action == Action::NextTab { 1 } else { 5 }) % 6;
                }
                Action::MoveLeft | Action::MoveRight | Action::ToggleSelect
                    if self.form.as_ref().is_some_and(|f| f.focus == 5) =>
                {
                    let form = self.form.as_mut().expect("form open");
                    form.engine = if form.engine == TemplateEngine::OpInject {
                        TemplateEngine::Custom
                    } else {
                        TemplateEngine::OpInject
                    };
                }
                _ => {}
            }
            return Ok(ScreenAction::None);
        }
        if self.deleting {
            match action {
                Action::Yes | Action::Confirm => {
                    if let Some(index) = self.owned_index() {
                        TemplateService::delete_template(ctx.repo_path, ctx.active_profile, index)?;
                        self.reload(ctx)?;
                        self.message = "Template removed; output file retained".to_string();
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
            Action::Create if self.worker.is_none() => self.form = Some(Form::new(None, None)),
            Action::Edit if self.worker.is_none() => {
                if let Some(index) = self.owned_index() {
                    self.form = Some(Form::new(self.selected(), Some(index)));
                } else if self.selected().is_some() {
                    self.message =
                        "Inherited template: edit its parent profile, or Add an override with the same destination"
                            .to_string();
                }
            }
            Action::Delete if self.worker.is_none() => {
                if self.owned_index().is_some() {
                    self.deleting = true;
                } else {
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
            .map(|template| {
                let exists = TemplateService::check_template(template).status == TemplateStatus::Rendered;
                let inherited = !self.owned.iter().any(|own| own.destination == template.destination);
                ListItem::new(format!(
                    "{} {}{}",
                    if exists { "✓" } else { "✗" },
                    template.name,
                    if inherited { " (inherited)" } else { "" }
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
        let detail = self.selected().map_or_else(|| "No templates yet. Add a committed source and choose its destination.".to_string(), |template| {
            let engine = if template.engine == TemplateEngine::OpInject { "op-inject (1Password CLI)" } else { "custom" };
            format!("{}\n\nEngine: {engine}\nSource: {}\nDestination: {}\n\n{}\n\n{}\n\nStatus checks file existence, not drift. Rendered files are not symlinked or synced.",
                template.name, template.source, template.destination, template.description.as_deref().unwrap_or(""), template.render_command.as_deref().unwrap_or(""))
        });
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
            let sections =
                Layout::vertical([Constraint::Min(3), Constraint::Length(2), Constraint::Length(3)]).split(inner);
            let visible = (usize::from(sections[0].height) / 3).clamp(1, 6);
            let start = form.focus.saturating_sub(visible - 1);
            let fields = Layout::vertical(vec![Constraint::Length(3); visible]).split(sections[0]);
            self.engine_area = Rect::default();
            for (row, i) in (start..(start + visible).min(6)).enumerate() {
                if i < 5 {
                    frame.render_text_input_widget(
                        TextInputWidget::new(&form.fields[i])
                            .title(LABELS[i])
                            .focused(form.focus == i)
                            .disabled(i == 3 && form.engine != TemplateEngine::Custom),
                        fields[row],
                    );
                    self.field_areas.push((fields[row], i));
                } else {
                    self.engine_area = fields[row];
                    let engine = if form.engine == TemplateEngine::OpInject {
                        "op-inject"
                    } else {
                        "custom"
                    };
                    frame.render_widget(
                        Paragraph::new(format!("Engine: {engine} (click or left/right to toggle)")).block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(if form.focus == 5 { t.primary } else { t.border })),
                        ),
                        fields[row],
                    );
                }
            }
            frame.render_widget(
                Paragraph::new(self.message.as_str()).wrap(Wrap { trim: true }),
                sections[1],
            );
            self.modal_buttons = Self::render_buttons(
                frame,
                sections[2],
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
                    if form.focus < 5
                        && !matches!(
                            action,
                            Some(Action::Save | Action::Confirm | Action::Cancel | Action::NextTab | Action::PrevTab)
                        )
                    {
                        if form.focus != 3 || form.engine == TemplateEngine::Custom {
                            form.fields[form.focus].handle_key(key.code);
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
                        match mouse.kind {
                            MouseEventKind::ScrollDown => form.focus = (form.focus + 1).min(5),
                            MouseEventKind::ScrollUp => form.focus = form.focus.saturating_sub(1),
                            _ => {}
                        }
                    }
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                        if let Some((_, action)) = self.modal_buttons.iter().find(|(rect, _)| rect.contains(point)) {
                            return self.action(*action, ctx);
                        }
                        if let Some(form) = self.form.as_mut() {
                            if let Some((_, index)) = self.field_areas.iter().find(|(rect, _)| rect.contains(point)) {
                                form.focus = *index;
                            }
                            if self.engine_area.contains(point) {
                                form.focus = 5;
                                form.engine = if form.engine == TemplateEngine::OpInject {
                                    TemplateEngine::Custom
                                } else {
                                    TemplateEngine::OpInject
                                };
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

    fn template(name: &str, dir: &std::path::Path) -> Template {
        Template {
            name: name.to_string(),
            source: "input.tpl".to_string(),
            destination: dir.join(format!("{name}.out")).to_string_lossy().into_owned(),
            engine: TemplateEngine::Custom,
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
        TemplateService::add_template(dir.path(), "parent", template("aaa inherited", dir.path())).unwrap();
        TemplateService::add_template(dir.path(), "child", template("zzz first", dir.path())).unwrap();
        TemplateService::add_template(dir.path(), "child", template("bbb second", dir.path())).unwrap();
        let config_path = dir.path().join("config.toml");
        let ctx = ScreenContext::new(&config, &config_path);
        let mut screen = ManageTemplatesScreen::new();
        screen.on_enter(&ctx).unwrap();
        screen.list.select(Some(1));
        screen.action(Action::Edit, &ctx).unwrap();
        screen.form.as_mut().unwrap().fields[0].set_text("edited second");
        screen.action(Action::Save, &ctx).unwrap();
        let manifest = ProfileManifest::load(dir.path()).unwrap();
        assert_eq!(manifest.profiles[1].templates[0].name, "zzz first");
        assert_eq!(manifest.profiles[1].templates[1].name, "edited second");
        screen.list.select(Some(1));
        screen.action(Action::Delete, &ctx).unwrap();
        screen.action(Action::Yes, &ctx).unwrap();
        let manifest = ProfileManifest::load(dir.path()).unwrap();
        assert_eq!(manifest.profiles[1].templates.len(), 1);
        assert_eq!(manifest.profiles[1].templates[0].name, "zzz first");
        screen.list.select(Some(0));
        screen.action(Action::Delete, &ctx).unwrap();
        assert!(!screen.deleting);
        assert_eq!(
            ProfileManifest::load(dir.path()).unwrap().profiles[0].templates.len(),
            1
        );
    }

    #[test]
    fn text_shortcuts_and_modal_mouse_input_do_not_activate_background_actions() {
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
        assert_eq!(screen.form.as_ref().unwrap().fields[0].text(), "qjed");
        draw(&mut screen, &config, 100, 32);
        let field = screen.field_areas[1].0;
        screen
            .handle_event(
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: field.x + 1,
                    row: field.y + 1,
                    modifiers: KeyModifiers::NONE,
                }),
                &ctx,
            )
            .unwrap();
        assert_eq!(screen.form.as_ref().unwrap().focus, 1);
        screen
            .handle_event(
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: field.x,
                    row: field.y,
                    modifiers: KeyModifiers::NONE,
                }),
                &ctx,
            )
            .unwrap();
        assert_eq!(screen.form.as_ref().unwrap().focus, 2);
        // Scrolling through the form keeps save/cancel visible even on small terminals.
        screen.form.as_mut().unwrap().focus = 5;
        draw(&mut screen, &config, 80, 20);
        assert!(screen.engine_area.height > 0);
        assert_eq!(screen.modal_buttons.len(), 2);
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
        assert_eq!(screen.templates.len(), 0);
    }

    #[test]
    fn add_form_saves_engine_source_and_destination_and_all_render_reports_failures() {
        let (dir, config) = setup();
        let config_path = dir.path().join("config.toml");
        let ctx = ScreenContext::new(&config, &config_path);
        let mut screen = ManageTemplatesScreen::new();
        screen.on_enter(&ctx).unwrap();
        screen.action(Action::Create, &ctx).unwrap();
        let form = screen.form.as_mut().unwrap();
        form.fields[0].set_text("fixture");
        form.fields[1].set_text("input.tpl");
        form.fields[2].set_text(dir.path().join("fixture.out").to_string_lossy());
        form.fields[3].set_text("true");
        form.focus = 5;
        screen.action(Action::MoveRight, &ctx).unwrap();
        screen.action(Action::Save, &ctx).unwrap();
        assert!(screen.form.is_none());
        let manifest = ProfileManifest::load(dir.path()).unwrap();
        let saved = &manifest.profiles[1].templates[0];
        assert_eq!(saved.source, "input.tpl");
        assert_eq!(saved.engine, TemplateEngine::Custom);
        assert_eq!(saved.render_command.as_deref(), Some("true"));
        screen.action(Action::SelectAll, &ctx).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while screen.worker.is_some() {
            assert!(Instant::now() < deadline, "render worker timed out");
            std::thread::sleep(Duration::from_millis(10));
            screen.tick();
        }
        assert!(screen.message.contains("1 of 1 failed"));
        assert!(screen.message.contains("destination file is missing"));
    }

    #[test]
    fn background_render_finishes_and_checks_output_without_symlinking_it() {
        let (dir, config) = setup();
        std::fs::write(dir.path().join("input.tpl"), "private fixture\n").unwrap();
        TemplateService::add_template(dir.path(), "child", template("rendered", dir.path())).unwrap();
        let config_path = dir.path().join("config.toml");
        let ctx = ScreenContext::new(&config, &config_path);
        let mut screen = ManageTemplatesScreen::new();
        screen.on_enter(&ctx).unwrap();
        screen.action(Action::Install, &ctx).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while screen.worker.is_some() {
            assert!(Instant::now() < deadline, "render worker timed out");
            std::thread::sleep(Duration::from_millis(10));
            screen.tick();
        }
        assert_eq!(screen.message, "Rendered 1 template(s)");
        let output = dir.path().join("rendered.out");
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "private fixture\n");
        assert!(!std::fs::symlink_metadata(&output).unwrap().file_type().is_symlink());
        draw(&mut screen, &config, 80, 24);
    }
}
