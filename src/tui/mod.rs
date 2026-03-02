//! TUI interface for rapid_driver using ratatui.
//!
//! Thin UI layer — all device management runs in the Engine.

mod events;

use crate::engine::command::{
    DeviceStatus, EngineCommand, EngineHandle, EngineResponse, RecordingFileEntry, RecordingInfo,
    ReplayState, ReplayStatusInfo,
};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use events::AppEvent;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const LOG_DEDUPE_SCAN_MULTIPLIER: usize = 8;
const LOG_RECENT_WINDOW_SECS: u64 = 12;

/// Application mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Monitor,
    Reorder,
    Export,
    Import,
    Replay,
}

/// Application state — thin UI wrapper around EngineHandle.
pub struct App {
    pub mode: AppMode,
    pub selected: usize,
    pub input_buffer: String,
    pub status_message: Option<(String, Instant)>,

    // Cached state from engine (refreshed ~10Hz)
    pub devices: Vec<DeviceStatus>,
    pub current_mask: u64,
    pub sequence: u64,
    pub measured_freq: f64,
    pub mask_enabled: bool,
    pub recording: Option<RecordingInfo>,

    // Replay state
    pub replay_recordings: Vec<RecordingFileEntry>,
    pub replay_selected: usize,
    pub replay_status: Option<ReplayStatusInfo>,

    // Engine handle
    handle: EngineHandle,

    // Log display
    log_dir: PathBuf,
    pub log_cache: HashMap<String, VecDeque<String>>,
    pub log_last_change: HashMap<String, Instant>,
    max_log_lines: usize,
}

impl App {
    pub fn new(handle: EngineHandle, mask_enabled: bool) -> Self {
        let log_dir = dirs::state_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("rapid_driver")
            .join("logs");

        // Get initial status from engine
        let (devices, current_mask, sequence, measured_freq, recording, resolved_mask_enabled) =
            if let Ok(EngineResponse::Status(status)) = handle.request(EngineCommand::GetStatus) {
                (
                    status.devices,
                    status.current_mask,
                    status.sequence,
                    status.measured_freq,
                    status.recording,
                    status.mask_enabled,
                )
            } else {
                (vec![], 0, 0, 0.0, None, mask_enabled)
            };

        App {
            mode: AppMode::Monitor,
            selected: 0,
            input_buffer: String::new(),
            status_message: None,
            devices,
            current_mask,
            sequence,
            measured_freq,
            mask_enabled: resolved_mask_enabled,
            recording,
            replay_recordings: Vec::new(),
            replay_selected: 0,
            replay_status: None,
            handle,
            log_dir,
            log_cache: HashMap::new(),
            log_last_change: HashMap::new(),
            max_log_lines: 20,
        }
    }

    fn dedupe_recent_lines(lines: &[String], limit: usize) -> Vec<String> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out_rev: Vec<String> = Vec::new();

        for line in lines.iter().rev() {
            let normalized = line.trim().to_string();
            if normalized.is_empty() {
                continue;
            }
            if seen.insert(normalized) {
                out_rev.push(line.clone());
                if out_rev.len() >= limit {
                    break;
                }
            }
        }

        out_rev.reverse();
        out_rev
    }

    /// Refresh cached state from engine.
    fn refresh_status(&mut self) {
        if let Ok(EngineResponse::Status(status)) = self.handle.request(EngineCommand::GetStatus) {
            self.devices = status.devices;
            self.current_mask = status.current_mask;
            self.sequence = status.sequence;
            self.measured_freq = status.measured_freq;
            self.mask_enabled = status.mask_enabled;
            self.recording = status.recording;
        }
        self.refresh_replay_status();
    }

    fn set_status(&mut self, msg: String) {
        self.status_message = Some((msg, Instant::now()));
    }

    fn get_status(&self) -> Option<&str> {
        self.status_message.as_ref().and_then(|(msg, time)| {
            if time.elapsed() < Duration::from_secs(5) {
                Some(msg.as_str())
            } else {
                None
            }
        })
    }

    fn restart_selected(&mut self) {
        let Some(device) = self.devices.get(self.selected) else {
            return;
        };
        let name = device.name.clone();
        match self
            .handle
            .request(EngineCommand::RestartDevice { name: name.clone() })
        {
            Ok(EngineResponse::Ok) => self.set_status(format!("{}: restarted", name)),
            Ok(EngineResponse::Error(e)) => self.set_status(format!("{}: {}", name, e)),
            _ => self.set_status(format!("{}: engine error", name)),
        }
    }

    fn move_selected_up(&mut self) {
        if self.selected > 0 {
            let _ = self.handle.request(EngineCommand::MoveDevice {
                from: self.selected,
                to: self.selected - 1,
            });
            self.selected -= 1;
            self.refresh_status();
        }
    }

    fn move_selected_down(&mut self) {
        if self.selected < self.devices.len().saturating_sub(1) {
            let _ = self.handle.request(EngineCommand::MoveDevice {
                from: self.selected,
                to: self.selected + 1,
            });
            self.selected += 1;
            self.refresh_status();
        }
    }

    fn save_layout(&mut self) {
        match self.handle.request(EngineCommand::SaveLayout) {
            Ok(EngineResponse::Ok) => self.set_status("Layout saved".into()),
            Ok(EngineResponse::Error(e)) => self.set_status(format!("Error: {}", e)),
            _ => self.set_status("Engine error".into()),
        }
    }

    fn export_layout(&mut self, path: &str) {
        match self.handle.request(EngineCommand::ExportLayout {
            path: path.to_string(),
        }) {
            Ok(EngineResponse::Ok) => self.set_status(format!("Exported to {}", path)),
            Ok(EngineResponse::Error(e)) => self.set_status(format!("Export error: {}", e)),
            _ => self.set_status("Engine error".into()),
        }
    }

    fn import_layout(&mut self, path: &str) {
        match self.handle.request(EngineCommand::ImportLayout {
            path: path.to_string(),
        }) {
            Ok(EngineResponse::Ok) => {
                self.refresh_status();
                self.set_status(format!("Imported from {}", path));
            }
            Ok(EngineResponse::Error(e)) => self.set_status(format!("Import error: {}", e)),
            _ => self.set_status("Engine error".into()),
        }
    }

    fn toggle_recording(&mut self) {
        let is_active = self
            .recording
            .as_ref()
            .is_some_and(|r| r.state == "recording" || r.state == "starting");

        if is_active {
            match self
                .handle
                .request(EngineCommand::StopRecording { session_id: None })
            {
                Ok(EngineResponse::Ok) => self.set_status("Recording stopping...".into()),
                Ok(EngineResponse::Error(e)) => self.set_status(format!("Stop error: {}", e)),
                _ => self.set_status("Engine error".into()),
            }
        } else {
            // All devices (recorder + data nodes) must be online and running
            if self.devices.is_empty() {
                self.set_status("Cannot record: no devices registered".into());
                return;
            }

            let not_ready: Vec<&str> = self
                .devices
                .iter()
                .filter(|d| !(d.process_running && d.heartbeat_ok.unwrap_or(true)))
                .map(|d| d.name.as_str())
                .collect();

            if !not_ready.is_empty() {
                self.set_status(format!(
                    "Cannot record: not connected: {}",
                    not_ready.join(", ")
                ));
                return;
            }

            match self.handle.request(EngineCommand::StartRecording {
                session_id: None,
                devices: None,
                output_dir: None,
            }) {
                Ok(EngineResponse::Ok) => self.set_status("Recording started".into()),
                Ok(EngineResponse::Error(e)) => self.set_status(format!("REC: {}", e)),
                _ => self.set_status("Engine error".into()),
            }
        }
    }

    fn fetch_recordings(&mut self) {
        match self.handle.request(EngineCommand::ListRecordings {
            sort: Some("created_at".into()),
            order: Some("desc".into()),
        }) {
            Ok(EngineResponse::RecordingsList { recordings, .. }) => {
                self.replay_recordings = recordings;
                self.replay_selected = 0;
            }
            Ok(EngineResponse::Error(e)) => {
                self.set_status(format!("List recordings: {}", e));
                self.replay_recordings.clear();
            }
            _ => {
                self.set_status("Failed to list recordings".into());
                self.replay_recordings.clear();
            }
        }
    }

    fn start_replay(&mut self) {
        let Some(entry) = self.replay_recordings.get(self.replay_selected) else {
            return;
        };
        let session_id = entry.session_id.clone();
        match self.handle.request(EngineCommand::StartReplay {
            session_id: session_id.clone(),
            speed: Some(1.0),
        }) {
            Ok(EngineResponse::ReplayStarted { .. }) => {
                self.set_status(format!("Replay started: {}", session_id));
                self.mode = AppMode::Monitor;
            }
            Ok(EngineResponse::Error(e)) => self.set_status(format!("Replay: {}", e)),
            _ => self.set_status("Replay: engine error".into()),
        }
    }

    fn stop_replay(&mut self) {
        match self.handle.request(EngineCommand::StopReplay) {
            Ok(EngineResponse::Ok) => self.set_status("Replay stopped".into()),
            Ok(EngineResponse::Error(e)) => self.set_status(format!("Stop replay: {}", e)),
            _ => self.set_status("Stop replay: engine error".into()),
        }
        self.refresh_replay_status();
    }

    fn refresh_replay_status(&mut self) {
        match self.handle.request(EngineCommand::GetReplayStatus) {
            Ok(EngineResponse::ReplayStatus(info)) => {
                self.replay_status = Some(info);
            }
            _ => {
                self.replay_status = None;
            }
        }
    }

    fn update_log_cache(&mut self, device_name: &str) {
        let log_path = self.log_dir.join(format!("{}.log", device_name));
        if let Ok(file) = File::open(&log_path) {
            let reader = BufReader::new(file);
            let lines: Vec<String> = reader.lines().filter_map(|l| l.ok()).collect();
            let scan_window = self
                .max_log_lines
                .saturating_mul(LOG_DEDUPE_SCAN_MULTIPLIER);
            let start = lines.len().saturating_sub(scan_window);
            let deduped = Self::dedupe_recent_lines(&lines[start..], self.max_log_lines);

            let cache = self
                .log_cache
                .entry(device_name.to_string())
                .or_insert_with(VecDeque::new);

            let changed = cache.len() != deduped.len() || !cache.iter().eq(deduped.iter());
            if changed {
                cache.clear();
                for line in deduped {
                    cache.push_back(line);
                }
                self.log_last_change
                    .insert(device_name.to_string(), Instant::now());
            }
        } else {
            self.log_cache.remove(device_name);
            self.log_last_change.remove(device_name);
        }
    }
}

/// Run the TUI application. Engine must already be running.
pub fn run_tui(handle: EngineHandle, mask_enabled: bool) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(handle, mask_enabled);

    let result = run_app(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> io::Result<()> {
    let mut last_status_update = Instant::now();
    let mut last_log_update = Instant::now();

    loop {
        terminal.draw(|f| ui(f, app))?;

        // Poll keyboard (100ms timeout for responsive UI)
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match handle_key(app, key.code) {
                        AppEvent::Quit => return Ok(()),
                        AppEvent::Continue => {}
                    }
                }
            }
        }

        // Refresh engine status (~10Hz)
        if last_status_update.elapsed() >= Duration::from_millis(100) {
            app.refresh_status();
            last_status_update = Instant::now();
        }

        // Update log cache (~1Hz)
        if last_log_update.elapsed() >= Duration::from_secs(1) {
            let device_names: Vec<String> = app.devices.iter().map(|d| d.name.clone()).collect();
            for name in device_names {
                app.update_log_cache(&name);
            }
            last_log_update = Instant::now();
        }
    }
}

fn handle_key(app: &mut App, key: KeyCode) -> AppEvent {
    match app.mode {
        AppMode::Monitor => match key {
            KeyCode::Char('q') => AppEvent::Quit,
            KeyCode::Char('r') => {
                app.mode = AppMode::Reorder;
                AppEvent::Continue
            }
            KeyCode::Char('e') => {
                app.mode = AppMode::Export;
                app.input_buffer.clear();
                AppEvent::Continue
            }
            KeyCode::Char('i') => {
                app.mode = AppMode::Import;
                app.input_buffer.clear();
                AppEvent::Continue
            }
            KeyCode::Char('R') => {
                app.toggle_recording();
                AppEvent::Continue
            }
            KeyCode::Char('P') => {
                // If replay is active, stop it; otherwise enter replay selection mode
                let is_replay_active = app
                    .replay_status
                    .as_ref()
                    .is_some_and(|s| matches!(s.state, ReplayState::Starting | ReplayState::Running));
                if is_replay_active {
                    app.stop_replay();
                } else {
                    app.fetch_recordings();
                    app.mode = AppMode::Replay;
                }
                AppEvent::Continue
            }
            KeyCode::Char('s') | KeyCode::Enter => {
                app.restart_selected();
                AppEvent::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if app.selected > 0 {
                    app.selected -= 1;
                }
                AppEvent::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if app.selected < app.devices.len().saturating_sub(1) {
                    app.selected += 1;
                }
                AppEvent::Continue
            }
            _ => AppEvent::Continue,
        },
        AppMode::Reorder => match key {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.mode = AppMode::Monitor;
                AppEvent::Continue
            }
            KeyCode::Enter => {
                app.save_layout();
                app.mode = AppMode::Monitor;
                AppEvent::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.move_selected_up();
                AppEvent::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.move_selected_down();
                AppEvent::Continue
            }
            _ => AppEvent::Continue,
        },
        AppMode::Replay => match key {
            KeyCode::Esc => {
                app.mode = AppMode::Monitor;
                AppEvent::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if app.replay_selected > 0 {
                    app.replay_selected -= 1;
                }
                AppEvent::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if app.replay_selected < app.replay_recordings.len().saturating_sub(1) {
                    app.replay_selected += 1;
                }
                AppEvent::Continue
            }
            KeyCode::Enter => {
                app.start_replay();
                AppEvent::Continue
            }
            _ => AppEvent::Continue,
        },
        AppMode::Export | AppMode::Import => match key {
            KeyCode::Esc => {
                app.mode = AppMode::Monitor;
                AppEvent::Continue
            }
            KeyCode::Enter => {
                let path = app.input_buffer.clone();
                if !path.is_empty() {
                    if app.mode == AppMode::Export {
                        app.export_layout(&path);
                    } else {
                        app.import_layout(&path);
                    }
                }
                app.mode = AppMode::Monitor;
                AppEvent::Continue
            }
            KeyCode::Backspace => {
                app.input_buffer.pop();
                AppEvent::Continue
            }
            KeyCode::Char(c) => {
                app.input_buffer.push(c);
                AppEvent::Continue
            }
            _ => AppEvent::Continue,
        },
    }
}

fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(f.area());

    // Header
    let mask_str = format!("0x{:08x}", app.current_mask);
    let freq_str = if app.mask_enabled {
        format!(" | {:.1} Hz", app.measured_freq)
    } else {
        String::new()
    };

    let mut header_spans = vec![
        Span::styled(
            " RAPID Hardware Mask Monitor ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | Mask: "),
        Span::styled(mask_str, Style::default().fg(Color::Yellow)),
        Span::raw(&freq_str),
    ];

    // Show recording indicator
    if let Some(ref rec) = app.recording {
        if rec.state == "recording" || rec.state == "starting" {
            header_spans.push(Span::raw(" | "));
            header_spans.push(Span::styled(
                format!(" REC {} ", rec.session_id),
                Style::default().fg(Color::White).bg(Color::Red),
            ));
        }
    }

    // Show replay indicator
    if let Some(ref rs) = app.replay_status {
        if matches!(rs.state, ReplayState::Starting | ReplayState::Running) {
            let sid = rs.session_id.as_deref().unwrap_or("?");
            let short_id = if sid.len() > 8 { &sid[..8] } else { sid };
            header_spans.push(Span::raw(" | "));
            header_spans.push(Span::styled(
                format!(" \u{25b6} REPLAY {} ", short_id),
                Style::default().fg(Color::White).bg(Color::Magenta),
            ));
        }
    }

    let header =
        Paragraph::new(Line::from(header_spans)).block(Block::default().borders(Borders::ALL));
    f.render_widget(header, chunks[0]);

    // Main content
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);

    // Left side: Devices + Recorder + Replay
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(8),
            Constraint::Length(6),
        ])
        .split(main_chunks[0]);

    render_device_list(f, app, left_chunks[0]);
    render_recorder_panel(f, app, left_chunks[1]);
    render_replay_panel(f, app, left_chunks[2]);

    // Right side: Mask + Log
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(5)])
        .split(main_chunks[1]);

    render_mask_display(f, app, right_chunks[0]);
    render_log_preview(f, app, right_chunks[1]);

    // Footer
    let help_text = match app.mode {
        AppMode::Monitor => "[q]uit [s/Enter]restart [r]eorder [e]xport [i]mport [R]ec [P]replay [↑↓]select",
        AppMode::Reorder => "[↑↓]move [Enter]save [Esc]cancel",
        AppMode::Export => "Enter path to export: ",
        AppMode::Import => "Enter path to import: ",
        AppMode::Replay => "[↑↓]select [Enter]start [Esc]back",
    };

    let footer_content = if matches!(app.mode, AppMode::Export | AppMode::Import) {
        format!("{}{}", help_text, app.input_buffer)
    } else if let Some(status) = app.get_status() {
        format!("{} | {}", help_text, status)
    } else {
        help_text.to_string()
    };

    let footer = Paragraph::new(footer_content).block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}

fn render_device_list(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .devices
        .iter()
        .enumerate()
        .map(|(i, device)| {
            let ready = device.process_running && device.heartbeat_ok.unwrap_or(true);
            let status_icon = if ready {
                Span::styled("●", Style::default().fg(Color::Green))
            } else if device.process_running || device.discovered {
                Span::styled("◐", Style::default().fg(Color::Yellow))
            } else {
                Span::styled("○", Style::default().fg(Color::Red))
            };

            let pid_str = device
                .pid
                .map(|p| format!("PID:{}", p))
                .unwrap_or_else(|| "-".to_string());

            let backend_tag = match device.backend.as_str() {
                "mdns" => Span::styled("[m]", Style::default().fg(Color::Magenta)),
                _ => Span::styled("[u]", Style::default().fg(Color::Blue)),
            };

            let line = Line::from(vec![
                Span::raw(format!("[{:>2}] ", device.bit_index)),
                status_icon,
                Span::raw(" "),
                backend_tag,
                Span::raw(format!(" {:<14} ", device.name)),
                Span::styled(pid_str, Style::default().fg(Color::DarkGray)),
            ]);

            let style = if i == app.selected {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };

            ListItem::new(line).style(style)
        })
        .collect();

    let title = match app.mode {
        AppMode::Reorder => " Devices (REORDER MODE) ",
        _ => " Devices ",
    };

    let list = List::new(items).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(if app.mode == AppMode::Reorder {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            }),
    );

    f.render_widget(list, area);
}

fn render_mask_display(f: &mut Frame, app: &App, area: Rect) {
    let mask = app.current_mask;
    let mut lines: Vec<Line> = Vec::new();

    let device_count = app.devices.len().min(8);
    let binary_str = format!("0b{:08b}", mask & 0xFF);

    let mut blocks: Vec<Span> = Vec::new();
    for i in 0..device_count {
        let bit_on = (mask >> i) & 1 == 1;
        let symbol = if bit_on {
            Span::styled("■", Style::default().fg(Color::Green))
        } else {
            Span::styled("□", Style::default().fg(Color::DarkGray))
        };
        blocks.push(symbol);
        if i < device_count - 1 {
            blocks.push(Span::raw(" "));
        }
    }

    let mut line1_spans = vec![
        Span::styled(&binary_str, Style::default().fg(Color::Yellow)),
        Span::raw("  "),
    ];
    line1_spans.extend(blocks);
    lines.push(Line::from(line1_spans));

    let mut labels = String::from("           ");
    for i in 0..device_count {
        labels.push_str(&format!("{} ", i));
    }
    lines.push(Line::from(Span::styled(
        labels,
        Style::default().fg(Color::DarkGray),
    )));

    lines.push(Line::from(""));

    let hex_str = format!("Hex: 0x{:016x}", mask);
    lines.push(Line::from(hex_str));

    let stats = format!("Seq: {}  Freq: {:.1} Hz", app.sequence, app.measured_freq);
    lines.push(Line::from(stats));

    let block = Block::default().title(" Mask ").borders(Borders::ALL);

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn render_log_preview(f: &mut Frame, app: &App, area: Rect) {
    let selected_device = app.devices.get(app.selected);
    let device_name = selected_device.map(|d| d.name.as_str()).unwrap_or("(none)");

    let logs = app
        .log_cache
        .get(device_name)
        .map(|q| q.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let is_recent = app
        .log_last_change
        .get(device_name)
        .is_some_and(|t| t.elapsed() <= Duration::from_secs(LOG_RECENT_WINDOW_SECS));

    let lines: Vec<Line> = if logs.is_empty() {
        vec![Line::from(Span::styled(
            "(no output)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else if !is_recent {
        vec![Line::from(Span::styled(
            "(no recent output)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        logs.iter()
            .map(|l| {
                let display = if l.len() > 80 {
                    format!("{}...", &l[..77])
                } else {
                    l.clone()
                };
                Line::from(display)
            })
            .collect()
    };

    let block = Block::default()
        .title(format!(" Log: {} ", device_name))
        .borders(Borders::ALL);

    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    f.render_widget(paragraph, area);
}

fn render_recorder_panel(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    match &app.recording {
        Some(rec) if rec.state != "stopped" && rec.state != "failed" || rec.state == "stopping" => {
            // State indicator
            let (icon, color) = match rec.state.as_str() {
                "recording" => ("●", Color::Green),
                "starting" => ("◐", Color::Yellow),
                "stopping" => ("◐", Color::Yellow),
                _ => ("○", Color::DarkGray),
            };

            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", icon), Style::default().fg(color)),
                Span::styled(
                    &rec.state,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ]));

            // Session ID
            let short_id = if rec.session_id.len() > 8 {
                &rec.session_id[..8]
            } else {
                &rec.session_id
            };
            lines.push(Line::from(format!("  Session: {}", short_id)));

            // Elapsed time and message count
            let elapsed = rec.elapsed_secs;
            let msgs = rec.mcap_message_count.unwrap_or(0);
            lines.push(Line::from(format!(
                "  Time: {:.1}s  Msgs: {}",
                elapsed, msgs
            )));

            // Output path
            lines.push(Line::from(vec![
                Span::raw("  Dir: "),
                Span::styled(&rec.output_dir, Style::default().fg(Color::DarkGray)),
            ]));

            // Separator
            lines.push(Line::from(Span::styled(
                "  ─────────────────────",
                Style::default().fg(Color::DarkGray),
            )));

            // Per-device status
            let is_active = rec.state == "recording" || rec.state == "starting";
            for (name, state) in &rec.devices {
                let (dev_icon, dev_color) = match state.as_str() {
                    "running" => ("●", Color::Green),
                    "pending" => ("◐", Color::Yellow),
                    "done" if is_active => ("●", Color::Green),
                    "done" => ("○", Color::DarkGray),
                    "failed" => ("✕", Color::Red),
                    _ => ("○", Color::DarkGray),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {} ", dev_icon), Style::default().fg(dev_color)),
                    Span::raw(format!("{:<14} ", name)),
                    Span::styled(state.as_str(), Style::default().fg(dev_color)),
                ]));
            }
        }
        _ => {
            lines.push(Line::from(Span::styled(
                "  (no active recording)",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    let block = Block::default().title(" Recorder ").borders(Borders::ALL);

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn render_replay_panel(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    if app.mode == AppMode::Replay {
        // Show recording list for selection
        if app.replay_recordings.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no recordings found)",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            let max_visible = area.height.saturating_sub(2) as usize; // borders
            let start = app
                .replay_selected
                .saturating_sub(max_visible.saturating_sub(1));
            for (i, entry) in app
                .replay_recordings
                .iter()
                .enumerate()
                .skip(start)
                .take(max_visible)
            {
                let short_id = if entry.session_id.len() > 12 {
                    &entry.session_id[..12]
                } else {
                    &entry.session_id
                };
                let dur = entry
                    .duration_secs
                    .map(|d| format!("{:.0}s", d))
                    .unwrap_or_else(|| "?".into());
                let text = format!(" {} ({})", short_id, dur);
                let style = if i == app.replay_selected {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };
                lines.push(Line::from(Span::styled(text, style)));
            }
        }

        let block = Block::default()
            .title(" Replay (SELECT) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Magenta));
        let paragraph = Paragraph::new(lines).block(block);
        f.render_widget(paragraph, area);
    } else if let Some(ref rs) = app.replay_status {
        if matches!(
            rs.state,
            ReplayState::Starting | ReplayState::Running
        ) {
            let sid = rs.session_id.as_deref().unwrap_or("?");
            let short_id = if sid.len() > 12 { &sid[..12] } else { sid };
            let pct = (rs.progress * 100.0).min(100.0);
            let elapsed = rs.elapsed_secs;
            let total = rs.total_secs;

            lines.push(Line::from(vec![
                Span::styled(" \u{25b6} ", Style::default().fg(Color::Magenta)),
                Span::styled(
                    match rs.state {
                        ReplayState::Starting => "starting",
                        ReplayState::Running => "playing",
                        _ => "idle",
                    },
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(format!("  Session: {}", short_id)));
            lines.push(Line::from(format!(
                "  {:.0}% | {:.1}s / {:.1}s",
                pct, elapsed, total
            )));

            let block = Block::default().title(" Replay ").borders(Borders::ALL);
            let paragraph = Paragraph::new(lines).block(block);
            f.render_widget(paragraph, area);
        } else {
            lines.push(Line::from(Span::styled(
                "  (idle)",
                Style::default().fg(Color::DarkGray),
            )));
            let block = Block::default().title(" Replay ").borders(Borders::ALL);
            let paragraph = Paragraph::new(lines).block(block);
            f.render_widget(paragraph, area);
        }
    } else {
        lines.push(Line::from(Span::styled(
            "  (idle)",
            Style::default().fg(Color::DarkGray),
        )));
        let block = Block::default().title(" Replay ").borders(Borders::ALL);
        let paragraph = Paragraph::new(lines).block(block);
        f.render_widget(paragraph, area);
    }
}
