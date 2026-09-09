//! The current-workspace Portboard terminal panel.
//!
//! The panel is a ratatui TUI shown in a Herdr popup pane (or any terminal via
//! `portboard panel`). A target list on the left drives a detail pane on the
//! right that lists every endpoint as its own untruncated, clickable/copyable
//! row, which is more room than the previous fzf picker could offer for URLs
//! and future rich interactions.
//!
//! Keys:
//!   enter            open or start the selected target / open the selected port
//!   o                open the selected endpoint / primary URL in a browser
//!   y                copy the endpoint / primary URL to the clipboard (OSC 52)
//!   s                stop the selected target
//!   r                refresh the inspection now (also polls every 2s)
//!   tab / ← / →      cycle focus between targets, ports, and endpoints
//!   ↑↓ / jk          move the cursor in the focused list
//!
//! The focused pane is always drawn with a bright cyan border and title while
//! the other panes are dimmed, and its selected row is reversed; unfocused
//! panes keep their cursor visible but subdued.
//!   mouse            click a row to select it; click an endpoint or port to open it
//!   q / esc          close the panel (esc first steps focus back)

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::browser_url::{browser_url, copy_url_to_clipboard, open_browser_url, UrlOpenOutcome};
use crate::current_workspace_status::{
    inspect_worktree_launch_targets_with_snapshot, CurrentLaunchTargetStatus,
    LaunchTargetRuntimeState, WorktreeLaunchTargetInspection,
};
use crate::herdr_workspace::current_herdr_workspace_id;
use crate::launch_target_config::load_worktree_launch_targets;
use crate::launch_target_open::open_or_start_launch_target;
use crate::launch_target_processes::DiscoverySnapshot;
use crate::launch_target_stop::{stop_launch_target, stop_launch_target_and_close_herdr_tab};
use crate::worktree_processes::{
    inspect_worktree_processes_with_snapshot, WorktreeProcessInventoryEntry,
};

/// How often the panel re-inspects targets so running/stopped status stays live.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const KEY_HINTS: &str =
    "enter open · o open url · y copy · s stop · r refresh · tab lists · q/esc close";

/// Opens the current worktree launch targets in a terminal panel.
pub fn open_current_workspace_panel(worktree_root: &Path) -> Result<()> {
    let mut app = PanelApp::new(worktree_root)?;
    app.refresh();

    enable_raw_mode().context("Portboard panel could not enable raw mode")?;
    let _raw_guard = RawModeGuard;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("Portboard panel could not enter the alternate screen")?;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = (|| -> Result<()> {
        terminal.hide_cursor()?;
        let outcome = app.run(&mut terminal);
        terminal.show_cursor()?;
        outcome
    })();
    // Restore the normal screen even when the event loop errored.
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    );
    result
}

/// Re-enables raw mode if the panel exits through an early error path.
struct RawModeGuard;
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// One of the three navigable lists in the panel.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Targets,
    Ports,
    Endpoints,
}

impl Focus {
    fn name(self) -> &'static str {
        match self {
            Focus::Targets => "targets",
            Focus::Ports => "ports",
            Focus::Endpoints => "endpoints",
        }
    }
}

/// One actionable URL shown in the detail pane.
struct EndpointRow {
    label: String,
    url: String,
}

/// One discovered listener in the worktree, shown whether or not a manifest
/// claims its process.
struct PortRow {
    label: String,
    url: String,
    pid: u32,
    target: Option<String>,
    port: u16,
}

struct PanelApp {
    worktree_root: PathBuf,
    inspection: Option<WorktreeLaunchTargetInspection>,
    inventory: Option<Vec<WorktreeProcessInventoryEntry>>,
    load_error: Option<String>,
    selected_target: usize,
    selected_port: usize,
    selected_endpoint: usize,
    focus: Focus,
    target_list_state: ListState,
    port_list_state: ListState,
    endpoint_list_state: ListState,
    message: Option<String>,
    last_refresh: Instant,
    stop_rx: Option<Receiver<Result<Vec<u32>>>>,
    open_url: fn(&str) -> String,
}

impl PanelApp {
    fn new(worktree_root: &Path) -> Result<Self> {
        let root = worktree_root.canonicalize().with_context(|| {
            format!(
                "Portboard could not resolve worktree {}",
                worktree_root.display()
            )
        })?;
        Ok(Self {
            worktree_root: root,
            inspection: None,
            inventory: None,
            load_error: None,
            selected_target: 0,
            selected_port: 0,
            selected_endpoint: 0,
            focus: Focus::Targets,
            target_list_state: ListState::default(),
            port_list_state: ListState::default(),
            endpoint_list_state: ListState::default(),
            message: None,
            last_refresh: Instant::now(),
            stop_rx: None,
            open_url: open_message,
        })
    }

    fn has_manifest(&self) -> bool {
        self.worktree_root.join("portboard.toml").is_file()
    }

    /// Re-inspects the current worktree. Loaded data is replaced; a failing
    /// manifest leaves the previous inspection in place and records an error.
    /// The process inventory is always refreshed so live ports stay visible
    /// with or without a `portboard.toml`.
    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        self.load_error = None;
        let snapshot = match DiscoverySnapshot::capture(&self.worktree_root) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.load_error = Some(format!("{error:#}"));
                return;
            }
        };
        let mut targets = Vec::new();
        if self.has_manifest() {
            match load_worktree_launch_targets(&self.worktree_root) {
                Ok(config) => {
                    targets = config.launch_targets.clone();
                    match inspect_worktree_launch_targets_with_snapshot(
                        &self.worktree_root,
                        &config,
                        &snapshot,
                    ) {
                        Ok(inspection) => self.inspection = Some(inspection),
                        Err(error) => self.load_error = Some(format!("{error:#}")),
                    }
                }
                Err(error) => {
                    self.inspection = None;
                    self.load_error = Some(format!("{error:#}"));
                }
            }
        } else {
            self.inspection = None;
        }
        match inspect_worktree_processes_with_snapshot(&targets, &snapshot) {
            Ok(inventory) => self.inventory = Some(inventory),
            Err(_) => {
                self.inventory = None;
                if self.load_error.is_none() {
                    self.load_error = Some("process scan failed".to_string());
                }
            }
        }
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        let target_count = self.target_count();
        if self.selected_target >= target_count {
            self.selected_target = target_count.saturating_sub(1);
        }
        let port_count = self.port_rows().len();
        if self.selected_port >= port_count {
            self.selected_port = port_count.saturating_sub(1);
        }
        let endpoint_count = self.endpoint_rows().len();
        if self.selected_endpoint >= endpoint_count {
            self.selected_endpoint = endpoint_count.saturating_sub(1);
        }
    }

    /// Every listener discovered in the worktree, sorted by port then pid.
    fn port_rows(&self) -> Vec<PortRow> {
        let Some(inventory) = &self.inventory else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for entry in inventory {
            for endpoint in &entry.endpoints {
                let url =
                    crate::browser_url::listener_browser_url(&endpoint.address, endpoint.port);
                rows.push(PortRow {
                    label: format!(":{}", endpoint.port),
                    url,
                    pid: entry.pid,
                    target: entry.target_ids.first().cloned(),
                    port: endpoint.port,
                });
            }
        }
        rows.sort_by_key(|row| (row.port, row.pid));
        rows.dedup_by(|a, b| a.url == b.url && a.pid == b.pid);
        rows
    }

    fn target_count(&self) -> usize {
        self.inspection
            .as_ref()
            .map(|inspection| inspection.statuses.len())
            .filter(|count| *count > 0)
            .unwrap_or_else(|| self.inventory.as_ref().map_or(0, Vec::len))
    }

    fn has_targets(&self) -> bool {
        self.inspection
            .as_ref()
            .is_some_and(|i| !i.statuses.is_empty())
    }

    fn selected_status(&self) -> Option<&CurrentLaunchTargetStatus> {
        self.inspection
            .as_ref()
            .and_then(|inspection| inspection.statuses.get(self.selected_target))
    }

    fn endpoint_rows(&self) -> Vec<EndpointRow> {
        let Some(status) = self.selected_status() else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for endpoint in &status.named_endpoints {
            rows.push(EndpointRow {
                label: endpoint.id.clone(),
                url: endpoint.url.clone(),
            });
        }
        if rows.is_empty() {
            for endpoint in &status.endpoints {
                let address = if endpoint.address.contains(':') {
                    format!("[{}]", endpoint.address)
                } else {
                    endpoint.address.clone()
                };
                rows.push(EndpointRow {
                    label: format!("tcp {address}:{}", endpoint.port),
                    url: crate::browser_url::listener_browser_url(&endpoint.address, endpoint.port),
                });
            }
        }
        rows
    }

    fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        loop {
            terminal.draw(|frame| self.render(frame))?;
            let has_input = event::poll(Duration::from_millis(200))
                .context("Portboard panel could not poll for input")?;
            if has_input {
                let event = event::read().context("Portboard panel could not read input")?;
                if matches!(self.handle_event(event)?, Some(true)) {
                    return Ok(());
                }
            }
            if self.last_refresh.elapsed() >= REFRESH_INTERVAL {
                self.refresh();
            }
            if let Some(message) = self.poll_stop() {
                self.message = Some(message);
            }
        }
    }

    /// Returns `Ok(Some(true))` when the panel should close.
    fn handle_event(&mut self, event: Event) -> Result<Option<bool>> {
        match event {
            Event::Key(key)
                if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat =>
            {
                match key.code {
                    KeyCode::Char('q') => return Ok(Some(true)),
                    KeyCode::Esc => {
                        // esc steps focus back one list before closing.
                        self.focus = match self.focus {
                            Focus::Endpoints => Focus::Ports,
                            Focus::Ports => Focus::Targets,
                            Focus::Targets => return Ok(Some(true)),
                        };
                    }
                    KeyCode::Enter => match self.focus {
                        Focus::Endpoints => self.open_endpoint(self.selected_endpoint),
                        Focus::Ports => self.open_port(self.selected_port),
                        Focus::Targets => return self.open_or_start(),
                    },
                    KeyCode::Char('o') => match self.focus {
                        Focus::Endpoints => self.open_endpoint(self.selected_endpoint),
                        Focus::Ports => self.open_port(self.selected_port),
                        Focus::Targets => self.open_primary(),
                    },
                    KeyCode::Char('y') => match self.focus {
                        Focus::Endpoints => self.copy_endpoint(self.selected_endpoint),
                        Focus::Ports => self.copy_port(self.selected_port),
                        Focus::Targets => self.copy_primary(),
                    },
                    KeyCode::Char('s') | KeyCode::Char('S') => {
                        if self.focus == Focus::Targets {
                            self.start_stop();
                        }
                    }
                    KeyCode::Char('r') => self.refresh(),
                    KeyCode::Tab | KeyCode::Right | KeyCode::Left => self.toggle_focus(),
                    KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                    KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                    _ => {}
                }
                Ok(None)
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Resize(_, _) => Ok(None),
            _ => Ok(None),
        }
    }

    /// Cycles focus through targets, ports, and endpoints.
    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Targets => Focus::Ports,
            Focus::Ports if self.has_targets() => Focus::Endpoints,
            Focus::Ports => Focus::Targets,
            Focus::Endpoints => Focus::Targets,
        };
    }

    fn move_selection(&mut self, delta: i64) {
        match self.focus {
            Focus::Targets => {
                let len = self.target_count() as i64;
                let next = (self.selected_target as i64 + delta).clamp(0, (len - 1).max(0));
                self.selected_target = next as usize;
                self.selected_endpoint = 0;
            }
            Focus::Ports => {
                let len = self.port_rows().len() as i64;
                let next = (self.selected_port as i64 + delta).clamp(0, (len - 1).max(0));
                self.selected_port = next as usize;
            }
            Focus::Endpoints => {
                let len = self.endpoint_rows().len() as i64;
                let next = (self.selected_endpoint as i64 + delta).clamp(0, (len - 1).max(0));
                self.selected_endpoint = next as usize;
            }
        }
    }

    /// Opens or starts the selected target, then closes the panel.
    fn open_or_start(&mut self) -> Result<Option<bool>> {
        let Some(status) = self.selected_status().cloned() else {
            return Ok(None);
        };
        let config = load_worktree_launch_targets(&self.worktree_root);
        let (config, message) = match config {
            Ok(config) => (Some(config), format!("{}: open/start…", status.label)),
            Err(error) => {
                self.message = Some(format!("{error:#}"));
                return Ok(None);
            }
        };
        let config = config.expect("unreachable");
        self.message = Some(message);
        match open_or_start_launch_target(&self.worktree_root, &config, Some(status.id.as_str())) {
            Ok(()) => Ok(Some(true)),
            Err(error) => {
                self.message = Some(format!("{error:#}"));
                Ok(None)
            }
        }
    }

    fn open_endpoint(&mut self, index: usize) {
        if let Some(row) = self.endpoint_rows().get(index) {
            self.message = Some((self.open_url)(row.url.as_str()));
        }
    }

    fn open_port(&mut self, index: usize) {
        if let Some(row) = self.port_rows().get(index) {
            self.message = Some((self.open_url)(row.url.as_str()));
        }
    }

    fn open_primary(&mut self) {
        let Some(status) = self.selected_status() else {
            return;
        };
        match browser_url(status) {
            Some(url) => self.message = Some((self.open_url)(url.as_str())),
            None => self.message = Some("no endpoint URL to open".to_string()),
        }
    }

    fn copy_endpoint(&mut self, index: usize) {
        if let Some(row) = self.endpoint_rows().get(index) {
            self.message = Some(copy_message(row.url.as_str()));
        }
    }

    fn copy_port(&mut self, index: usize) {
        if let Some(row) = self.port_rows().get(index) {
            self.message = Some(copy_message(row.url.as_str()));
        }
    }

    fn copy_primary(&mut self) {
        let Some(status) = self.selected_status() else {
            return;
        };
        match browser_url(status) {
            Some(url) => self.message = Some(copy_message(url.as_str())),
            None => self.message = Some("no endpoint URL to copy".to_string()),
        }
    }

    fn start_stop(&mut self) {
        if self.stop_rx.is_some() {
            return;
        }
        let Ok(config) = load_worktree_launch_targets(&self.worktree_root) else {
            self.message = Some("could not reload manifest to stop".to_string());
            return;
        };
        let Some(status) = self.selected_status().cloned() else {
            return;
        };
        let Some(target) = config
            .launch_targets
            .iter()
            .find(|target| target.id.as_str() == status.id.as_str())
            .cloned()
        else {
            return;
        };
        let root = self.worktree_root.clone();
        let workspace_id = current_herdr_workspace_id();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = match workspace_id {
                Some(workspace_id) => {
                    stop_launch_target_and_close_herdr_tab(&root, &target, &workspace_id)
                }
                None => stop_launch_target(&root, &target),
            };
            let _ = sender.send(result);
        });
        self.stop_rx = Some(receiver);
        self.message = Some(format!("{}: stopping…", status.label));
    }

    fn poll_stop(&mut self) -> Option<String> {
        let receiver = self.stop_rx.as_ref()?;
        let message = match receiver.try_recv() {
            Ok(result) => match result {
                Ok(pids) if pids.is_empty() => "already stopped".to_string(),
                Ok(pids) => format!(
                    "stopping pid {}",
                    pids.iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                Err(error) => format!("{error:#}"),
            },
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => "stop worker disconnected".to_string(),
        };
        self.stop_rx = None;
        Some(message)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<Option<bool>> {
        let (width, height) =
            crossterm::terminal::size().context("Portboard panel could not read terminal size")?;
        let area = Rect::new(0, 0, width, height);
        self.handle_mouse_in(mouse, area)
    }

    fn handle_mouse_in(&mut self, mouse: MouseEvent, area: Rect) -> Result<Option<bool>> {
        let areas = layout_areas(area);
        let point = (mouse.column, mouse.row);
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
        ) {
            if rect_contains(areas.targets, point) {
                self.focus = Focus::Targets;
            } else if rect_contains(areas.ports, point) {
                self.focus = Focus::Ports;
            } else if rect_contains(areas.endpoints, point) && self.has_targets() {
                self.focus = Focus::Endpoints;
            }
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if rect_contains(areas.targets, point) {
                    let index = mouse.row.saturating_sub(areas.targets.y) as usize
                        + self.target_list_state.offset();
                    if index < self.target_count() {
                        self.selected_target = index;
                        self.selected_endpoint = 0;
                        self.focus = Focus::Targets;
                    }
                } else if rect_contains(areas.ports, point) {
                    let index = mouse.row.saturating_sub(areas.ports.y) as usize
                        + self.port_list_state.offset();
                    if index < self.port_rows().len() {
                        self.selected_port = index;
                        self.focus = Focus::Ports;
                        self.open_port(index);
                    }
                } else if rect_contains(areas.endpoints, point) {
                    let index = mouse.row.saturating_sub(areas.endpoints.y) as usize
                        + self.endpoint_list_state.offset();
                    if index < self.endpoint_rows().len() {
                        self.selected_endpoint = index;
                        self.focus = Focus::Endpoints;
                        self.open_endpoint(index);
                    }
                }
            }
            MouseEventKind::ScrollDown => self.move_selection(1),
            MouseEventKind::ScrollUp => self.move_selection(-1),
            _ => {}
        }
        Ok(None)
    }

    fn render(&mut self, frame: &mut Frame) {
        let areas = layout_areas(frame.area());
        self.target_list_state.select(Some(self.selected_target));
        self.port_list_state.select(Some(self.selected_port));
        self.endpoint_list_state
            .select(Some(self.selected_endpoint));

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " Portboard",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" · {}", self.worktree_root.display()),
                    Style::default().fg(Color::DarkGray),
                ),
            ])),
            areas.header,
        );

        self.render_manifest(frame, &areas);

        frame.render_widget(self.footer(), areas.footer);
    }

    fn render_manifest(&mut self, frame: &mut Frame, areas: &Areas) {
        let targets = List::new(self.target_items())
            .block(
                Block::new()
                    .borders(Borders::ALL)
                    .title(Span::styled(
                        if self.has_targets() {
                            " targets "
                        } else {
                            " processes "
                        },
                        self.pane_title_style(Focus::Targets),
                    ))
                    .border_style(self.pane_border_style(Focus::Targets)),
            )
            .highlight_style(self.pane_highlight_style(Focus::Targets))
            .highlight_symbol(if self.focus == Focus::Targets {
                "▸ "
            } else {
                "  "
            });
        frame.render_stateful_widget(targets, areas.targets_block, &mut self.target_list_state);

        let port_count = self.port_rows().len();
        let ports_title = format!(" ports ({port_count}) ");
        let ports = List::new(self.port_items())
            .block(
                Block::new()
                    .borders(Borders::ALL)
                    .title(Span::styled(
                        ports_title,
                        self.pane_title_style(Focus::Ports),
                    ))
                    .border_style(self.pane_border_style(Focus::Ports)),
            )
            .highlight_style(self.pane_highlight_style(Focus::Ports))
            .highlight_symbol(if self.focus == Focus::Ports {
                "▸ "
            } else {
                "  "
            });
        frame.render_stateful_widget(ports, areas.ports_block, &mut self.port_list_state);

        frame.render_widget(self.info_paragraph(), areas.info_block);

        let endpoints = List::new(self.endpoint_items())
            .block(
                Block::new()
                    .borders(Borders::ALL)
                    .title(Line::from(vec![
                        Span::styled(" endpoints ", self.pane_title_style(Focus::Endpoints)),
                        Span::styled(
                            "(click or enter to open · y to copy)",
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]))
                    .border_style(self.pane_border_style(Focus::Endpoints)),
            )
            .highlight_style(self.pane_highlight_style(Focus::Endpoints))
            .highlight_symbol(if self.focus == Focus::Endpoints {
                "▸ "
            } else {
                "  "
            });
        frame.render_stateful_widget(
            endpoints,
            areas.endpoints_block,
            &mut self.endpoint_list_state,
        );
    }

    fn inventory_items(&self) -> Vec<ListItem<'static>> {
        let inventory = self.inventory.clone().unwrap_or_default();
        let mut items = Vec::new();
        if inventory.is_empty() {
            items.push(ListItem::new(Line::from(Span::styled(
                "no live processes in this worktree",
                Style::default().fg(Color::DarkGray),
            ))));
        } else {
            for entry in inventory.iter() {
                let ports = if entry.endpoints.is_empty() {
                    String::new()
                } else {
                    format!(
                        "  {}",
                        entry
                            .endpoints
                            .iter()
                            .map(|endpoint| format!(":{}", endpoint.port))
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                };
                items.push(ListItem::new(Line::from(vec![
                    Span::styled(
                        pad(&entry.pid.to_string(), 7),
                        Style::default().fg(Color::Green),
                    ),
                    Span::styled(ports, Style::default().fg(Color::Cyan)),
                    Span::raw(format!("  {}", truncate(&entry.argv.join(" "), 80))),
                ])));
            }
        }
        items
    }

    fn target_items(&self) -> Vec<ListItem<'static>> {
        if !self.has_targets() {
            return self.inventory_items();
        }
        let inspection = self.inspection.as_ref().expect("targets inspection");
        inspection
            .statuses
            .iter()
            .map(|status| {
                let (word, color) = match &status.state {
                    LaunchTargetRuntimeState::Stopped => ("stopped", Color::DarkGray),
                    LaunchTargetRuntimeState::Running { .. } if status.duplicate => {
                        ("duplicate!", Color::Red)
                    }
                    LaunchTargetRuntimeState::Running { .. } => ("running", Color::Green),
                };
                let pid_text = match &status.state {
                    LaunchTargetRuntimeState::Running { processes } => format!(
                        "  pid {}",
                        processes
                            .iter()
                            .map(|process| process.pid.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    LaunchTargetRuntimeState::Stopped => String::new(),
                };
                ListItem::new(Line::from(vec![
                    Span::raw(pad(&status.label, 24)),
                    Span::styled(
                        if status.metadata_error.is_some() {
                            "warning! "
                        } else {
                            ""
                        },
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled(word, Style::default().fg(color)),
                    Span::styled(pid_text, Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect()
    }

    fn endpoint_items(&self) -> Vec<ListItem<'static>> {
        self.endpoint_rows()
            .into_iter()
            .map(|row| {
                ListItem::new(Line::from(vec![
                    Span::styled(pad(&row.label, 8), Style::default().fg(Color::Cyan)),
                    Span::raw(format!("  {}", row.url)),
                ]))
            })
            .collect()
    }

    fn port_items(&self) -> Vec<ListItem<'static>> {
        self.port_rows()
            .into_iter()
            .map(|row| {
                let mut spans = vec![
                    Span::styled(pad(&row.label, 7), Style::default().fg(Color::Cyan)),
                    Span::raw(format!("  {}", row.url)),
                    Span::styled(
                        format!("  pid {}", row.pid),
                        Style::default().fg(Color::DarkGray),
                    ),
                ];
                if let Some(target) = row.target {
                    spans.push(Span::styled(
                        format!(" · {target}"),
                        Style::default().fg(Color::Green),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect()
    }

    fn info_paragraph(&self) -> Paragraph<'static> {
        let status = self.selected_status();
        let mut lines = Vec::new();
        match status {
            Some(status) => {
                if let Some(error) = &status.metadata_error {
                    lines.push(Line::from(Span::styled(
                        error.clone(),
                        Style::default().fg(Color::Yellow),
                    )));
                }
                match &status.state {
                    LaunchTargetRuntimeState::Stopped => lines.push(Line::from(vec![
                        Span::styled("state", Style::default().fg(Color::DarkGray)),
                        Span::raw("  stopped"),
                    ])),
                    LaunchTargetRuntimeState::Running { processes } => {
                        let (word, color) = if status.duplicate {
                            (
                                format!("running · DUPLICATE ({} instances)", processes.len()),
                                Color::Red,
                            )
                        } else {
                            ("running".to_string(), Color::Green)
                        };
                        lines.push(Line::from(vec![
                            Span::styled("state", Style::default().fg(Color::DarkGray)),
                            Span::styled(format!("  {word}"), Style::default().fg(color)),
                            Span::styled(
                                format!(
                                    "  pid {}",
                                    processes
                                        .iter()
                                        .map(|process| process.pid.to_string())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ]));
                    }
                }
                lines.push(Line::from(vec![
                    Span::styled("argv ", Style::default().fg(Color::DarkGray)),
                    Span::raw(format!(" {}", status.argv.join(" "))),
                ]));
            }
            None => {
                if let Some(error) = &self.load_error {
                    lines.push(Line::from(Span::styled(
                        error.clone(),
                        Style::default().fg(Color::Red),
                    )));
                } else {
                    lines.push(Line::from(
                        "No launch targets; configure portboard.toml to manage runs.",
                    ));
                }
            }
        }
        let title = match status {
            Some(status) => format!(" {} ", status.label),
            None => " detail ".to_string(),
        };
        // The detail header belongs to the same right-hand column as the
        // endpoints list, so it lights up with the endpoints focus.
        Paragraph::new(lines)
            .block(
                Block::new()
                    .borders(Borders::ALL)
                    .title(Span::styled(title, self.pane_title_style(Focus::Endpoints)))
                    .border_style(self.pane_border_style(Focus::Endpoints)),
            )
            .wrap(Wrap { trim: false })
    }

    /// Border color for a pane: bright cyan when focused, dimmed otherwise.
    fn pane_border_style(&self, focus: Focus) -> Style {
        if self.focus == focus {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    }

    /// Title style for a pane: bold cyan when focused, dimmed otherwise.
    fn pane_title_style(&self, focus: Focus) -> Style {
        if self.focus == focus {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    }

    /// Selected-row style: reversed in the focused pane so the cursor pops,
    /// subdued dark gray elsewhere so it stays visible without competing.
    fn pane_highlight_style(&self, focus: Focus) -> Style {
        if self.focus == focus {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    }

    fn footer(&self) -> Paragraph<'static> {
        let mut lines = Vec::new();
        if let Some(message) = &self.message {
            lines.push(Line::from(Span::styled(
                format!(" {message}"),
                Style::default().fg(Color::Cyan),
            )));
        }
        let warnings = self
            .inspection
            .as_ref()
            .map(|i| i.findings.join("; "))
            .unwrap_or_default();
        if !warnings.is_empty() || self.load_error.is_some() {
            lines.insert(
                0,
                Line::from(Span::styled(
                    format!(
                        " warning: {}{}",
                        self.load_error.as_deref().unwrap_or(""),
                        warnings
                    ),
                    Style::default().fg(Color::Yellow),
                )),
            );
        }
        lines.push(Line::from(Span::styled(
            format!(
                " focus {} · ",
                if self.focus == Focus::Targets && !self.has_targets() {
                    "processes"
                } else {
                    self.focus.name()
                }
            ),
            Style::default().fg(Color::Cyan),
        )));
        lines.push(Line::from(Span::styled(
            format!(" {KEY_HINTS}"),
            Style::default().fg(Color::DarkGray),
        )));
        Paragraph::new(lines).block(Block::new())
    }
}

fn open_message(url: &str) -> String {
    match open_browser_url(url) {
        UrlOpenOutcome::OpenedLocally => format!("opens {url} in your browser"),
        UrlOpenOutcome::OfferedLink => {
            format!("ctrl-click {url} above to open it in your local browser")
        }
    }
}

fn copy_message(url: &str) -> String {
    if copy_url_to_clipboard(url) {
        format!("copied {url} to clipboard")
    } else {
        format!("{url} (terminal does not support OSC 52 copy; select the text instead)")
    }
}

fn pad(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(value.chars().count());
    format!("{value}{}", " ".repeat(padding))
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    let mut truncated = value
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

struct Areas {
    header: Rect,
    footer: Rect,
    targets_block: Rect,
    targets: Rect,
    ports_block: Rect,
    ports: Rect,
    info_block: Rect,
    endpoints_block: Rect,
    endpoints: Rect,
}

/// Splits the terminal into a header, body, and a fixed-height footer. The
/// body is then split into a left column (targets above the always-visible
/// worktree-wide ports list) and a right detail column, whose endpoints list
/// occupies the space below the detail header.
fn layout_areas(area: Rect) -> Areas {
    let footer_height = 3u16;
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(footer_height),
        ])
        .split(area);
    let (header, body, footer) = (vertical[0], vertical[1], vertical[2]);

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
        .split(body);
    let (left, right) = (horizontal[0], horizontal[1]);

    let left_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(left);
    let (targets_block, ports_block) = (left_rows[0], left_rows[1]);
    let targets = inner_rect(targets_block);
    let ports = inner_rect(ports_block);

    let detail = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(0)])
        .split(right);
    let (info_block, endpoints_block) = (detail[0], detail[1]);
    let endpoints = inner_rect(endpoints_block);

    Areas {
        header,
        footer,
        targets_block,
        targets,
        ports_block,
        ports,
        info_block,
        endpoints_block,
        endpoints,
    }
}

fn inner_rect(block: Rect) -> Rect {
    Rect {
        x: block.x + 1,
        y: block.y + 1,
        width: block.width.saturating_sub(2),
        height: block.height.saturating_sub(2),
    }
}

fn rect_contains(rect: Rect, (x, y): (u16, u16)) -> bool {
    x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_endpoints::ProcessEndpoint;
    use crossterm::event::{KeyEvent, KeyModifiers};
    fn app(root: &Path) -> PanelApp {
        let mut app = PanelApp::new(root).unwrap();
        app.open_url = |url| format!("opened {url}");
        app.inventory = Some(
            (0..40)
                .map(|i| WorktreeProcessInventoryEntry {
                    pid: 100 + i,
                    argv: vec![format!("process-{i}")],
                    target_ids: vec![],
                    endpoints: vec![ProcessEndpoint {
                        protocol: "tcp",
                        address: "0.0.0.0".into(),
                        port: 8000 + i as u16,
                    }],
                })
                .collect(),
        );
        app
    }
    #[test]
    fn inventory_and_ports_remain_visible_and_scrollable() {
        for manifest in [
            None,
            Some("version = 1\nlaunch_targets = []"),
            Some("invalid"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            if let Some(text) = manifest {
                std::fs::write(dir.path().join("portboard.toml"), text).unwrap();
            }
            let mut app = app(dir.path());
            app.move_selection(30);
            assert_eq!(app.selected_target, 30);
            let backend = ratatui::backend::TestBackend::new(120, 30);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| app.render(f)).unwrap();
            let text = format!("{:?}", terminal.backend().buffer());
            assert!(text.contains("ports (40)"), "{text}");
            assert!(text.contains("process-30"), "{text}");
            assert!(app.target_list_state.offset() > 0);
        }
    }
    #[test]
    fn enter_on_ports_opens_selected_url_not_target() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.focus = Focus::Ports;
        app.selected_port = 3;
        // No browser or Herdr operation is allowed in this test.
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert!(app
            .message
            .as_deref()
            .unwrap_or("")
            .contains("http://127.0.0.1:8003"));
    }
    #[test]
    fn mouse_indices_include_each_list_scroll_offset() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let config = crate::launch_target_config::parse_launch_target_config(
            "version = 1\n[[launch_targets]]\nid = 'x'\nlabel = 'X'\nargv = ['not-running']",
        )
        .unwrap();
        app.inspection = Some(
            inspect_worktree_launch_targets_with_snapshot(
                dir.path(),
                &config,
                &DiscoverySnapshot::capture(dir.path()).unwrap(),
            )
            .unwrap(),
        );
        app.inspection.as_mut().unwrap().statuses[0].endpoints = app
            .inventory
            .as_ref()
            .unwrap()
            .iter()
            .flat_map(|e| e.endpoints.clone())
            .collect();
        let area = Rect::new(0, 0, 120, 30);
        let areas = layout_areas(area);
        for (focus, rect) in [
            (Focus::Ports, areas.ports),
            (Focus::Endpoints, areas.endpoints),
        ] {
            *app.port_list_state.offset_mut() = 9;
            *app.endpoint_list_state.offset_mut() = 9;
            app.handle_mouse_in(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: rect.x,
                    row: rect.y + 1,
                    modifiers: KeyModifiers::NONE,
                },
                area,
            )
            .unwrap();
            assert!(app.focus == focus);
            assert!(app.message.as_deref().unwrap().contains(":8010"));
        }
        app.inspection = None;
        *app.target_list_state.offset_mut() = 9;
        app.handle_mouse_in(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: areas.targets.x,
                row: areas.targets.y + 1,
                modifiers: KeyModifiers::NONE,
            },
            area,
        )
        .unwrap();
        assert_eq!(app.selected_target, 10);
    }
    #[test]
    fn refresh_and_render_keep_invalid_manifest_warning_and_ports() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("portboard.toml"), "invalid").unwrap();
        let mut app = app(dir.path());
        app.refresh();
        assert!(app.load_error.is_some());
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text = format!("{:?}", terminal.backend().buffer());
        assert!(text.contains("ports ("), "{text}");
        assert!(text.contains("warning:"), "{text}");
        assert!(text.contains("processes"), "{text}");
        app.toggle_focus();
        assert!(app.focus == Focus::Ports);
        app.toggle_focus();
        assert!(app.focus == Focus::Targets);
    }
}
