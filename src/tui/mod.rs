//! TUI interface for rapid_driver using ratatui.

mod events;

use crate::mask::{DebugPublisher, MaskLayout, MaskPublisher};
use crate::mask::debug::{DebugState, DeviceDebugInfo};
use crate::registry::{self, DeviceEntry, Registry};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use events::AppEvent;
use nix::libc;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::os::unix::io::AsFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Running process information
struct RunningProcess {
    child: Child,
    entry_name: String,
    on_detach: String,
    entry: DeviceEntry,
    restart_count: u32,
    last_restart: Instant,
}

/// Application mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    /// Normal monitoring mode
    Monitor,
    /// Reordering device bit positions
    Reorder,
    /// Export dialog
    Export,
    /// Import dialog
    Import,
}

/// Device status for display
#[derive(Debug, Clone)]
pub struct DeviceStatus {
    pub name: String,
    pub bit_index: usize,
    pub usb_connected: bool,
    pub process_running: bool,
    pub pid: Option<u32>,
}

/// Application state
pub struct App {
    /// Current mode
    pub mode: AppMode,
    /// Registry
    pub registry: Registry,
    /// Mask layout
    pub layout: MaskLayout,
    /// Selected device index
    pub selected: usize,
    /// Current mask value
    pub current_mask: u64,
    /// Sequence number
    pub sequence: u64,
    /// Device statuses
    pub devices: Vec<DeviceStatus>,
    /// Running processes (sysname -> process)
    running: HashMap<String, RunningProcess>,
    /// Connected USB devices (sysname -> device_name)
    connected_usb: HashMap<String, String>,
    /// Cooldown timestamps
    cooldown: HashMap<String, Instant>,
    /// Log directory
    log_dir: PathBuf,
    /// Recent log lines per device (device_name -> lines)
    pub log_cache: HashMap<String, VecDeque<String>>,
    /// Maximum log lines to cache
    max_log_lines: usize,
    /// Mask publisher
    mask_publisher: Option<MaskPublisher>,
    /// Debug publisher
    debug_publisher: Option<DebugPublisher>,
    /// Mask publishing enabled
    mask_enabled: bool,
    /// File path input buffer (for export/import)
    pub input_buffer: String,
    /// Status message
    pub status_message: Option<(String, Instant)>,
    /// Frequency measurement
    pub publish_count: u64,
    pub last_freq_check: Instant,
    pub measured_freq: f64,
}

/// Cooldown duration in seconds
const COOLDOWN_SECS: u64 = 2;
/// Maximum restart attempts
const MAX_RESTART_COUNT: u32 = 5;
/// Restart window in seconds
const RESTART_WINDOW_SECS: u64 = 60;
/// Restart delay in milliseconds
const RESTART_DELAY_MS: u64 = 2000;

impl App {
    /// Create a new App instance.
    pub fn new(registry: Registry, mask_enabled: bool) -> Self {
        let log_dir = dirs::state_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("rapid_driver")
            .join("logs");

        let _ = fs::create_dir_all(&log_dir);

        let device_names = registry.device_names_sorted();
        let layout = MaskLayout::load_or_default(&device_names);

        let (mask_publisher, debug_publisher) = if mask_enabled {
            let mp = MaskPublisher::new().ok();
            let dp = DebugPublisher::new().ok();
            (mp, dp)
        } else {
            (None, None)
        };

        // Initialize device statuses
        let devices: Vec<DeviceStatus> = layout
            .device_order
            .iter()
            .enumerate()
            .map(|(bit_index, name)| DeviceStatus {
                name: name.clone(),
                bit_index,
                usb_connected: false,
                process_running: false,
                pid: None,
            })
            .collect();

        App {
            mode: AppMode::Monitor,
            registry,
            layout,
            selected: 0,
            current_mask: 0,
            sequence: 0,
            devices,
            running: HashMap::new(),
            connected_usb: HashMap::new(),
            cooldown: HashMap::new(),
            log_dir,
            log_cache: HashMap::new(),
            max_log_lines: 10,
            mask_publisher,
            debug_publisher,
            mask_enabled,
            input_buffer: String::new(),
            status_message: None,
            publish_count: 0,
            last_freq_check: Instant::now(),
            measured_freq: 0.0,
        }
    }

    /// Enumerate and start attached devices.
    pub fn enumerate_and_start(&mut self) {
        let mut enumerator = udev::Enumerator::new().expect("failed to create enumerator");
        enumerator
            .match_subsystem("usb")
            .expect("failed to match subsystem");

        let devices = enumerator.scan_devices().expect("failed to scan devices");

        for device in devices {
            let devtype = device.property_value("DEVTYPE");
            if devtype.and_then(|v| v.to_str()) != Some("usb_device") {
                continue;
            }
            if let Some(identity) = registry::extract_identity(&device) {
                if let Some(entry) = registry::find_match(&self.registry, &identity).cloned() {
                    self.connected_usb
                        .insert(identity.sysname.clone(), entry.name.clone());
                    self.spawn_attach(&identity.sysname, &entry);
                }
            }
        }

        self.update_device_statuses();
        self.publish_mask();
    }

    /// Handle udev event.
    pub fn handle_udev_event(&mut self, device: &udev::Device, action: &str) {
        let devtype = device.property_value("DEVTYPE");
        if devtype.and_then(|v| v.to_str()) != Some("usb_device") {
            return;
        }

        let sysname = device.sysname().to_string_lossy().into_owned();

        match action {
            "add" => {
                if let Some(&last_remove) = self.cooldown.get(&sysname) {
                    if last_remove.elapsed() < Duration::from_secs(COOLDOWN_SECS) {
                        return;
                    }
                    self.cooldown.remove(&sysname);
                }
                let identity = match registry::extract_identity(device) {
                    Some(id) => id,
                    None => return,
                };
                if let Some(entry) = registry::find_match(&self.registry, &identity).cloned() {
                    self.connected_usb
                        .insert(identity.sysname.clone(), entry.name.clone());
                    self.spawn_attach(&identity.sysname, &entry);
                    self.set_status(format!("[+] {} connected", entry.name));
                }
            }
            "remove" => {
                if let Some(name) = self.connected_usb.remove(&sysname) {
                    self.set_status(format!("[-] {} disconnected", name));
                }
                if self.running.contains_key(&sysname) {
                    self.terminate_process(&sysname);
                    self.cooldown.insert(sysname, Instant::now());
                }
            }
            _ => {}
        }

        self.update_device_statuses();
    }

    fn spawn_attach(&mut self, sysname: &str, entry: &DeviceEntry) {
        self.spawn_attach_with_count(sysname, entry, 0);
    }

    fn spawn_attach_with_count(&mut self, sysname: &str, entry: &DeviceEntry, restart_count: u32) {
        if self.running.contains_key(sysname) || entry.on_attach.is_empty() {
            return;
        }

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&entry.on_attach).process_group(0);

        let log_path = self.log_dir.join(format!("{}.log", entry.name));
        if let Ok(log_file) = File::create(&log_path) {
            let log_file_err = log_file.try_clone().unwrap_or_else(|_| {
                File::create("/dev/null").unwrap()
            });
            cmd.stdout(Stdio::from(log_file));
            cmd.stderr(Stdio::from(log_file_err));
        }

        if let Ok(child) = cmd.spawn() {
            self.running.insert(
                sysname.to_string(),
                RunningProcess {
                    child,
                    entry_name: entry.name.clone(),
                    on_detach: entry.on_detach.clone(),
                    entry: entry.clone(),
                    restart_count,
                    last_restart: Instant::now(),
                },
            );
        }
    }

    fn terminate_process(&mut self, sysname: &str) {
        let Some(mut proc) = self.running.remove(sysname) else {
            return;
        };

        let pid = proc.child.id();

        match proc.child.try_wait() {
            Ok(Some(_)) => {}
            _ => {
                let pgid = pid as i32;
                unsafe {
                    libc::kill(-pgid, libc::SIGTERM);
                }

                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match proc.child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) => {
                            if Instant::now() >= deadline {
                                unsafe {
                                    libc::kill(-pgid, libc::SIGKILL);
                                }
                                let _ = proc.child.wait();
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(500));

        if !proc.on_detach.is_empty() {
            let _ = Command::new("sh")
                .arg("-c")
                .arg(&proc.on_detach)
                .status();
        }
    }

    /// Kill all running processes.
    pub fn kill_all(&mut self) {
        let sysnames: Vec<String> = self.running.keys().cloned().collect();
        for sysname in sysnames {
            self.terminate_process(&sysname);
        }
    }

    /// Reap exited processes.
    pub fn reap_exited(&mut self) {
        let mut exited = Vec::new();
        let mut to_restart = Vec::new();

        for (sysname, proc) in self.running.iter_mut() {
            if let Ok(Some(_status)) = proc.child.try_wait() {
                if device_still_exists(sysname) {
                    let current_count = if proc.last_restart.elapsed()
                        > Duration::from_secs(RESTART_WINDOW_SECS)
                    {
                        0
                    } else {
                        proc.restart_count
                    };

                    if current_count < MAX_RESTART_COUNT {
                        to_restart.push((sysname.clone(), proc.entry.clone(), current_count + 1));
                    }
                }
                exited.push(sysname.clone());
            }
        }

        for sysname in &exited {
            self.running.remove(sysname);
        }

        for (sysname, entry, restart_count) in to_restart {
            std::thread::sleep(Duration::from_millis(RESTART_DELAY_MS));
            self.spawn_attach_with_count(&sysname, &entry, restart_count);
        }

        if !exited.is_empty() {
            self.update_device_statuses();
        }
    }

    /// Update device statuses from current state.
    fn update_device_statuses(&mut self) {
        let connected_names: HashSet<&String> = self.connected_usb.values().collect();
        let running_pids: HashMap<&String, u32> = self
            .running
            .values()
            .map(|p| (&p.entry_name, p.child.id()))
            .collect();

        for device in &mut self.devices {
            device.usb_connected = connected_names.contains(&device.name);
            device.process_running = running_pids.contains_key(&device.name);
            device.pid = running_pids.get(&device.name).copied();
        }
    }

    /// Compute mask value.
    fn compute_mask(&self) -> u64 {
        let mut mask: u64 = 0;
        for device in &self.devices {
            if device.usb_connected && device.process_running && device.bit_index < 64 {
                mask |= 1u64 << device.bit_index;
            }
        }
        mask
    }

    /// Publish mask to shared memory.
    pub fn publish_mask(&mut self) {
        self.current_mask = self.compute_mask();

        if let Some(ref mut publisher) = self.mask_publisher {
            let device_count = self.devices.len() as u8;
            if publisher.publish(self.current_mask, device_count).is_ok() {
                self.sequence = publisher.sequence();
                self.publish_count += 1;
            }
        }

        // Update frequency measurement
        let elapsed = self.last_freq_check.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.measured_freq = self.publish_count as f64 / elapsed.as_secs_f64();
            self.publish_count = 0;
            self.last_freq_check = Instant::now();
        }
    }

    /// Publish debug state (rate-limited).
    pub fn publish_debug(&mut self) {
        let Some(ref mut debug_pub) = self.debug_publisher else {
            return;
        };

        if !debug_pub.should_publish() {
            return;
        }

        let mut state = DebugState::new(self.sequence);
        state.set_mask(self.current_mask, self.devices.len());

        for device in &self.devices {
            state.add_device(
                device.name.clone(),
                DeviceDebugInfo {
                    bit: device.bit_index,
                    online: device.usb_connected && device.process_running,
                    usb_connected: device.usb_connected,
                    process_running: device.process_running,
                    pid: device.pid,
                },
            );
        }

        let _ = debug_pub.publish(&state);
    }

    /// Update log cache for a device.
    pub fn update_log_cache(&mut self, device_name: &str) {
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

    /// Set status message.
    pub fn set_status(&mut self, msg: String) {
        self.status_message = Some((msg, Instant::now()));
    }

    /// Get current status message if not expired.
    pub fn get_status(&self) -> Option<&str> {
        self.status_message.as_ref().and_then(|(msg, time)| {
            if time.elapsed() < Duration::from_secs(5) {
                Some(msg.as_str())
            } else {
                None
            }
        })
    }

    /// Move selected device up in layout.
    pub fn move_selected_up(&mut self) {
        if self.selected > 0 {
            self.layout.move_device(self.selected, self.selected - 1);
            self.selected -= 1;
            self.rebuild_devices();
        }
    }

    /// Move selected device down in layout.
    pub fn move_selected_down(&mut self) {
        if self.selected < self.devices.len().saturating_sub(1) {
            self.layout.move_device(self.selected, self.selected + 1);
            self.selected += 1;
            self.rebuild_devices();
        }
    }

    /// Rebuild device list from layout.
    fn rebuild_devices(&mut self) {
        let connected_names: HashSet<&String> = self.connected_usb.values().collect();
        let running_pids: HashMap<&String, u32> = self
            .running
            .values()
            .map(|p| (&p.entry_name, p.child.id()))
            .collect();

        self.devices = self
            .layout
            .device_order
            .iter()
            .enumerate()
            .map(|(bit_index, name)| DeviceStatus {
                name: name.clone(),
                bit_index,
                usb_connected: connected_names.contains(name),
                process_running: running_pids.contains_key(name),
                pid: running_pids.get(name).copied(),
            })
            .collect();
    }

    /// Manually restart the selected device's process.
    /// Resets restart count and spawns the process if USB is connected.
    pub fn restart_selected(&mut self) {
        let Some(device) = self.devices.get(self.selected) else {
            return;
        };
        let device_name = device.name.clone();

        // Find the sysname for this device
        let sysname = self
            .connected_usb
            .iter()
            .find(|(_, name)| *name == &device_name)
            .map(|(sys, _)| sys.clone());

        let Some(sysname) = sysname else {
            self.set_status(format!("{}: USB not connected", device_name));
            return;
        };

        // Kill existing process if any
        if self.running.contains_key(&sysname) {
            self.terminate_process(&sysname);
        }

        // Find the registry entry
        let entry = self
            .registry
            .devices
            .iter()
            .find(|e| e.name == device_name)
            .cloned();

        if let Some(entry) = entry {
            // Spawn with reset restart count
            self.spawn_attach_with_count(&sysname, &entry, 0);
            self.set_status(format!("{}: manually restarted", device_name));
            self.update_device_statuses();
        } else {
            self.set_status(format!("{}: not in registry", device_name));
        }
    }

    /// Save layout to config.
    pub fn save_layout(&mut self) {
        if let Err(e) = self.layout.save() {
            self.set_status(format!("Error saving layout: {}", e));
        } else {
            self.set_status("Layout saved".into());
        }
    }

    /// Export layout to file.
    pub fn export_layout(&mut self, path: &str) {
        if let Err(e) = self.layout.export(path) {
            self.set_status(format!("Export error: {}", e));
        } else {
            self.set_status(format!("Exported to {}", path));
        }
    }

    /// Import layout from file.
    pub fn import_layout(&mut self, path: &str) {
        match MaskLayout::import(path) {
            Ok(layout) => {
                self.layout = layout;
                self.rebuild_devices();
                if let Err(e) = self.layout.save() {
                    self.set_status(format!("Imported but save failed: {}", e));
                } else {
                    self.set_status(format!("Imported from {}", path));
                }
            }
            Err(e) => {
                self.set_status(format!("Import error: {}", e));
            }
        }
    }
}

fn device_still_exists(sysname: &str) -> bool {
    let path = format!("/sys/bus/usb/devices/{}", sysname);
    std::path::Path::new(&path).exists()
}

/// Run the TUI application.
pub fn run_tui(registry: Registry, mask_enabled: bool) -> io::Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app
    let mut app = App::new(registry, mask_enabled);
    app.enumerate_and_start();

    // Setup udev monitor
    let socket = udev::MonitorBuilder::new()
        .expect("failed to create monitor builder")
        .match_subsystem_devtype("usb", "usb_device")
        .expect("failed to set subsystem filter")
        .listen()
        .expect("failed to listen on monitor");

    let udev_fd = socket.as_fd();

    // Main loop
    let result = run_app(&mut terminal, &mut app, &socket, udev_fd);

    // Cleanup
    app.kill_all();
    if mask_enabled {
        MaskPublisher::cleanup();
        DebugPublisher::cleanup();
    }

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
    socket: &udev::MonitorSocket,
    udev_fd: std::os::unix::io::BorrowedFd,
) -> io::Result<()> {
    let mut last_log_update = Instant::now();
    let mut loop_count: u64 = 0;

    loop {
        let iteration_start = Instant::now();

        // Draw UI at reduced rate (100Hz) when mask enabled, every iteration otherwise
        if !app.mask_enabled || loop_count % 5 == 0 {
            terminal.draw(|f| ui(f, app))?;
        }

        // Non-blocking poll when mask enabled (for 500Hz), blocking otherwise
        let poll_timeout = if app.mask_enabled {
            Duration::ZERO
        } else {
            Duration::from_millis(100)
        };

        // Check for keyboard events
        if event::poll(poll_timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match handle_key(app, key.code) {
                        AppEvent::Quit => return Ok(()),
                        AppEvent::Continue => {}
                    }
                }
            }
        }

        // Check for udev events
        let mut poll_fds = [PollFd::new(udev_fd, PollFlags::POLLIN)];
        if let Ok(n) = poll(&mut poll_fds, PollTimeout::from(0u16)) {
            if n > 0 {
                if let Some(event) = socket.iter().next() {
                    let action = event
                        .action()
                        .and_then(|a| a.to_str())
                        .unwrap_or("unknown");
                    app.handle_udev_event(&event.device(), action);
                }
            }
        }

        // Publish mask
        app.publish_mask();
        app.publish_debug();

        // Reap exited processes
        app.reap_exited();

        // Update log cache periodically (1Hz)
        if last_log_update.elapsed() >= Duration::from_secs(1) {
            let device_names: Vec<String> = app.devices.iter().map(|d| d.name.clone()).collect();
            for name in device_names {
                app.update_log_cache(&name);
            }
            last_log_update = Instant::now();
        }

        // Precise timing for 500Hz when mask enabled
        if app.mask_enabled {
            let target = Duration::from_micros(2000); // 500Hz
            let elapsed = iteration_start.elapsed();
            if elapsed < target {
                let remaining = target - elapsed;
                // Sleep for most of the time (leave 200us for spin-wait)
                if remaining > Duration::from_micros(200) {
                    std::thread::sleep(remaining - Duration::from_micros(200));
                }
                // Spin-wait for precise timing
                while iteration_start.elapsed() < target {
                    std::hint::spin_loop();
                }
            }
        }

        loop_count = loop_count.wrapping_add(1);
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
            Constraint::Length(3),  // Header
            Constraint::Min(10),    // Main content
            Constraint::Length(3),  // Footer/help
        ])
        .split(f.area());

    // Header
    let mask_str = format!("0x{:08x}", app.current_mask);
    let freq_str = if app.mask_enabled {
        format!(" | {:.1} Hz", app.measured_freq)
    } else {
        String::new()
    };
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " RAPID Hardware Mask Monitor ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | Mask: "),
        Span::styled(mask_str, Style::default().fg(Color::Yellow)),
        Span::raw(&freq_str),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(header, chunks[0]);

    // Main content
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);

    // Device list
    render_device_list(f, app, main_chunks[0]);

    // Right panel: mask visualization + logs
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(5)])
        .split(main_chunks[1]);

    render_mask_display(f, app, right_chunks[0]);
    render_log_preview(f, app, right_chunks[1]);

    // Footer
    let help_text = match app.mode {
        AppMode::Monitor => "[q]uit [s/Enter]restart [r]eorder [e]xport [i]mport [↑↓]select",
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

    let footer = Paragraph::new(footer_content)
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}

fn render_device_list(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .devices
        .iter()
        .enumerate()
        .map(|(i, device)| {
            let status_icon = if device.usb_connected && device.process_running {
                Span::styled("●", Style::default().fg(Color::Green))
            } else if device.usb_connected {
                Span::styled("◐", Style::default().fg(Color::Yellow))
            } else {
                Span::styled("○", Style::default().fg(Color::Red))
            };

            let pid_str = device
                .pid
                .map(|p| format!("PID:{}", p))
                .unwrap_or_else(|| "-".to_string());

            let line = Line::from(vec![
                Span::raw(format!("[{:>2}] ", device.bit_index)),
                status_icon,
                Span::raw(format!(" {:<16} ", device.name)),
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

    // Show binary representation with device names
    // e.g., "0b00000011  [cam_main|cam_rear|------|......]"
    let device_count = app.devices.len().min(8);
    let binary_str = format!("0b{:08b}", mask & 0xFF);

    // Build visual blocks: ■ for online, □ for offline
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

    // Line 1: Binary value with visual blocks
    let mut line1_spans = vec![
        Span::styled(&binary_str, Style::default().fg(Color::Yellow)),
        Span::raw("  "),
    ];
    line1_spans.extend(blocks);
    lines.push(Line::from(line1_spans));

    // Line 2: Bit index labels
    let mut labels = String::from("           "); // align with "0b00000011  "
    for i in 0..device_count {
        labels.push_str(&format!("{} ", i));
    }
    lines.push(Line::from(Span::styled(labels, Style::default().fg(Color::DarkGray))));

    // Line 3: empty
    lines.push(Line::from(""));

    // Line 4: Hex value + stats
    let hex_str = format!("Hex: 0x{:016x}", mask);
    lines.push(Line::from(hex_str));

    // Line 5: Sequence and frequency
    let stats = format!("Seq: {}  Freq: {:.1} Hz", app.sequence, app.measured_freq);
    lines.push(Line::from(stats));

    let block = Block::default()
        .title(" Mask ")
        .borders(Borders::ALL);

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

    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    f.render_widget(paragraph, area);
}
