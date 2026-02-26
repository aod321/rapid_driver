//! TUI interface for rapid_driver using ratatui.
//!
//! Thin UI layer — all device management runs in the Engine.

mod events;

use crate::engine::command::{DeviceStatus, EngineCommand, EngineHandle, EngineResponse, RecordingInfo};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use events::AppEvent;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Application mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Monitor,
    Reorder,
    Export,
    Import,
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

    // Engine handle
    handle: EngineHandle,

    // Log display
    log_dir: PathBuf,
    pub log_cache: HashMap<String, VecDeque<String>>,
    max_log_lines: usize,
}

impl App {
    pub fn new(handle: EngineHandle, mask_enabled: bool) -> Self {
        let log_dir = dirs::state_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("rapid_driver")
            .join("logs");

        // Get initial status from engine
        let (devices, current_mask, sequence, measured_freq, recording) =
            if let Ok(EngineResponse::Status(status)) = handle.request(EngineCommand::GetStatus) {
                (
                    status.devices,
                    status.current_mask,
                    status.sequence,
                    status.measured_freq,
                    status.recording,
                )
            } else {
                (vec![], 0, 0, 0.0, None)
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
            mask_enabled,
            recording,
            handle,
            log_dir,
            log_cache: HashMap::new(),
            max_log_lines: 10,
        }
    }

    /// Refresh cached state from engine.
    fn refresh_status(&mut self) {
        if let Ok(EngineResponse::Status(status)) = self.handle.request(EngineCommand::GetStatus) {
            self.devices = status.devices;
            self.current_mask = status.current_mask;
            self.sequence = status.sequence;
            self.measured_freq = status.measured_freq;
            self.recording = status.recording;
        }
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
        match self.handle.request(EngineCommand::RestartDevice { name: name.clone() }) {
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
        match self.handle.request(EngineCommand::ExportLayout { path: path.to_string() }) {
            Ok(EngineResponse::Ok) => self.set_status(format!("Exported to {}", path)),
            Ok(EngineResponse::Error(e)) => self.set_status(format!("Export error: {}", e)),
            _ => self.set_status("Engine error".into()),
        }
    }

    fn import_layout(&mut self, path: &str) {
        match self.handle.request(EngineCommand::ImportLayout { path: path.to_string() }) {
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
            match self.handle.request(EngineCommand::StopRecording { session_id: None }) {
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
                .filter(|d| !d.online || !d.process_running)
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

    fn update_log_cache(&mut self, device_name: &str) {
        let log_path = self.log_dir.join(format!("{}.log", device_name));
        if let Ok(file) = File::open(&log_path) {
            let reader = BufReader::new(file);
            let lines: Vec<String> = reader.lines().filter_map(|l| l.ok()).collect();
            let start = lines.len().saturating_sub(self.max_log_lines);

            let cache = self
                .log_cache
                .entry(device_name.to_string())
                .or_insert_with(VecDeque::new);
            cache.clear();
            for line in &lines[start..] {
                cache.push_back(line.clone());
            }
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

    let header = Paragraph::new(Line::from(header_spans))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(header, chunks[0]);

    // Main content
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);

    // Left side: Devices + Recorder
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(8)])
        .split(main_chunks[0]);

    render_device_list(f, app, left_chunks[0]);
    render_recorder_panel(f, app, left_chunks[1]);

    // Right side: Mask + Log
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(5)])
        .split(main_chunks[1]);

    render_mask_display(f, app, right_chunks[0]);
    render_log_preview(f, app, right_chunks[1]);

    // Footer
    let help_text = match app.mode {
        AppMode::Monitor => "[q]uit [s/Enter]restart [r]eorder [e]xport [i]mport [R]ec [↑↓]select",
        AppMode::Reorder => "[↑↓]move [Enter]save [Esc]cancel",
        AppMode::Export => "Enter path to export: ",
        AppMode::Import => "Enter path to import: ",
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
            let status_icon = if device.online && device.process_running {
                Span::styled("●", Style::default().fg(Color::Green))
            } else if device.online {
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
    let device_name = selected_device
        .map(|d| d.name.as_str())
        .unwrap_or("(none)");

    let logs = app
        .log_cache
        .get(device_name)
        .map(|q| q.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();

    let lines: Vec<Line> = if logs.is_empty() {
        vec![Line::from(Span::styled(
            "(no output)",
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

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: true });
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
                Span::styled(&rec.state, Style::default().fg(color).add_modifier(Modifier::BOLD)),
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

    let block = Block::default()
        .title(" Recorder ")
        .borders(Borders::ALL);

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}
