//! Core engine: owns all runtime state, runs 500Hz main loop.

pub mod command;
pub mod recording;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, SyncSender};
use std::time::{Duration, Instant};

use nix::libc;

use crate::discovery::{DiscoveryEvent, DiscoverySource};
use crate::mask::debug::{DebugState, DeviceDebugInfo};
use crate::mask::{DebugPublisher, MaskLayout, MaskPublisher};
use crate::registry::{self, DeviceEntry, Registry};

use command::{
    DeviceStatus, EngineCommand, EngineResponse, EngineStatus, RecordingInfo,
};
use recording::{RecordingSession, RecordingState};

/// Running process information.
struct RunningProcess {
    child: Child,
    entry_name: String,
    on_detach: String,
    entry: DeviceEntry,
    restart_count: u32,
    last_restart: Instant,
}

/// Pending restart (non-blocking delay).
struct PendingRestart {
    source_id: String,
    entry: DeviceEntry,
    restart_count: u32,
    restart_at: Instant,
}

/// Maximum restart attempts per window.
const MAX_RESTART_COUNT: u32 = 5;
/// Restart window in seconds.
const RESTART_WINDOW_SECS: u64 = 60;
/// Restart delay in milliseconds.
const RESTART_DELAY_MS: u64 = 2000;

type CommandMsg = (EngineCommand, Option<SyncSender<EngineResponse>>);

pub struct Engine {
    // Registry & layout
    registry: Registry,
    layout: MaskLayout,

    // Device state
    devices: Vec<DeviceStatus>,
    running: HashMap<String, RunningProcess>,
    connected_devices: HashMap<String, String>, // source_id -> device_name
    device_addresses: HashMap<String, String>,  // device_name -> address

    // Discovery
    discovery_sources: Vec<Box<dyn DiscoverySource>>,

    // Mask publishing
    mask_enabled: bool,
    current_mask: u64,
    sequence: u64,
    mask_publisher: Option<MaskPublisher>,
    debug_publisher: Option<DebugPublisher>,
    publish_count: u64,
    last_freq_check: Instant,
    measured_freq: f64,

    // Logging
    log_dir: PathBuf,

    // Recording
    recording: Option<RecordingSession>,

    // Non-blocking restarts
    pending_restarts: Vec<PendingRestart>,

    // Command channel
    cmd_rx: Receiver<CommandMsg>,
}

impl Engine {
    pub fn new(
        registry: Registry,
        mask_enabled: bool,
        discovery_sources: Vec<Box<dyn DiscoverySource>>,
        cmd_rx: Receiver<CommandMsg>,
    ) -> Self {
        let log_dir = dirs::state_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("rapid_driver")
            .join("logs");
        let _ = fs::create_dir_all(&log_dir);

        let device_names = registry.device_names_sorted();
        let layout = MaskLayout::load_or_default(&device_names);

        let (mask_publisher, debug_publisher) = if mask_enabled {
            (MaskPublisher::new().ok(), DebugPublisher::new().ok())
        } else {
            (None, None)
        };

        let devices: Vec<DeviceStatus> = layout
            .device_order
            .iter()
            .enumerate()
            .map(|(bit_index, name)| {
                let backend = registry
                    .devices
                    .iter()
                    .find(|e| e.name == *name)
                    .map(|e| e.backend.clone())
                    .unwrap_or_else(|| "usb".to_string());
                DeviceStatus {
                    name: name.clone(),
                    bit_index,
                    online: false,
                    process_running: false,
                    pid: None,
                    backend,
                    address: None,
                }
            })
            .collect();

        Engine {
            registry,
            layout,
            devices,
            running: HashMap::new(),
            connected_devices: HashMap::new(),
            device_addresses: HashMap::new(),
            discovery_sources,
            mask_enabled,
            current_mask: 0,
            sequence: 0,
            mask_publisher,
            debug_publisher,
            publish_count: 0,
            last_freq_check: Instant::now(),
            measured_freq: 0.0,
            log_dir,
            recording: None,
            pending_restarts: Vec::new(),
            cmd_rx,
        }
    }

    /// Main loop. Blocks until Shutdown command received.
    pub fn run(&mut self) {
        self.enumerate_and_start();

        loop {
            let iteration_start = Instant::now();

            if !self.process_commands() {
                break;
            }

            self.poll_discovery();
            self.publish_mask();
            self.publish_debug();
            self.reap_exited();
            self.process_pending_restarts();
            self.check_recording();

            // Precise timing for 500Hz when mask enabled
            if self.mask_enabled {
                let target = Duration::from_micros(2000);
                let elapsed = iteration_start.elapsed();
                if elapsed < target {
                    let remaining = target - elapsed;
                    if remaining > Duration::from_micros(200) {
                        std::thread::sleep(remaining - Duration::from_micros(200));
                    }
                    while iteration_start.elapsed() < target {
                        std::hint::spin_loop();
                    }
                }
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        // Cleanup
        self.kill_all();
        self.shutdown_discovery();
        if self.mask_enabled {
            MaskPublisher::cleanup();
            DebugPublisher::cleanup();
        }
    }

    // ── Command processing ──────────────────────────────────────────

    /// Process pending commands. Returns false if shutdown requested.
    fn process_commands(&mut self) -> bool {
        while let Ok((cmd, resp_tx)) = self.cmd_rx.try_recv() {
            let is_shutdown = matches!(cmd, EngineCommand::Shutdown);
            let response = self.handle_command(cmd);
            if let Some(tx) = resp_tx {
                let _ = tx.send(response);
            }
            if is_shutdown {
                return false;
            }
        }
        true
    }

    fn handle_command(&mut self, cmd: EngineCommand) -> EngineResponse {
        match cmd {
            EngineCommand::Shutdown => EngineResponse::Ok,
            EngineCommand::GetStatus => EngineResponse::Status(self.build_status()),
            EngineCommand::GetDevices => EngineResponse::Devices(self.devices.clone()),
            EngineCommand::GetDevice { name } => {
                EngineResponse::Device(self.devices.iter().find(|d| d.name == name).cloned())
            }
            EngineCommand::RestartDevice { name } => self.cmd_restart_device(&name),
            EngineCommand::StopDevice { name } => self.cmd_stop_device(&name),
            EngineCommand::StartDevice { name } => self.cmd_start_device(&name),
            EngineCommand::GetRegistry => EngineResponse::Registry(self.registry.clone()),
            EngineCommand::AddDevice { entry } => self.cmd_add_device(entry),
            EngineCommand::RemoveDevice { name } => self.cmd_remove_device(&name),
            EngineCommand::GetMask => EngineResponse::Mask {
                value: self.current_mask,
                hex: format!("0x{:016x}", self.current_mask),
                layout: self.layout.device_order.clone(),
            },
            EngineCommand::StartRecording {
                session_id,
                devices,
                output_dir,
            } => self.cmd_start_recording(session_id, devices, output_dir),
            EngineCommand::StopRecording { session_id } => self.cmd_stop_recording(session_id),
            EngineCommand::GetRecordingStatus => {
                EngineResponse::RecordingStatus(self.build_recording_info())
            }
            EngineCommand::MoveDevice { from, to } => {
                self.layout.move_device(from, to);
                self.rebuild_devices();
                EngineResponse::Ok
            }
            EngineCommand::SaveLayout => match self.layout.save() {
                Ok(()) => EngineResponse::Ok,
                Err(e) => EngineResponse::Error(format!("save layout: {}", e)),
            },
            EngineCommand::ExportLayout { path } => match self.layout.export(&path) {
                Ok(()) => EngineResponse::Ok,
                Err(e) => EngineResponse::Error(format!("export: {}", e)),
            },
            EngineCommand::ImportLayout { path } => match MaskLayout::import(&path) {
                Ok(layout) => {
                    self.layout = layout;
                    self.rebuild_devices();
                    let _ = self.layout.save();
                    EngineResponse::Ok
                }
                Err(e) => EngineResponse::Error(format!("import: {}", e)),
            },
        }
    }

    fn build_status(&self) -> EngineStatus {
        EngineStatus {
            devices: self.devices.clone(),
            current_mask: self.current_mask,
            mask_hex: format!("0x{:016x}", self.current_mask),
            sequence: self.sequence,
            measured_freq: self.measured_freq,
            mask_enabled: self.mask_enabled,
            recording: self.build_recording_info(),
        }
    }

    fn build_recording_info(&self) -> Option<RecordingInfo> {
        self.recording.as_ref().map(|r| RecordingInfo {
            session_id: r.session_id.clone(),
            state: r.state.as_str().to_string(),
            devices: r.device_state_strings(),
            output_dir: r.output_dir.clone(),
            elapsed_secs: r.created_at.elapsed().as_secs_f64(),
        })
    }

    // ── Device commands ─────────────────────────────────────────────

    fn cmd_restart_device(&mut self, name: &str) -> EngineResponse {
        let source_id = self
            .connected_devices
            .iter()
            .find(|(_, n)| *n == name)
            .map(|(s, _)| s.clone());

        let Some(source_id) = source_id else {
            return EngineResponse::Error(format!("{}: not connected", name));
        };

        if self.running.contains_key(&source_id) {
            self.terminate_process(&source_id);
        }

        let entry = self.registry.devices.iter().find(|e| e.name == name).cloned();
        if let Some(entry) = entry {
            self.spawn_attach_with_count(&source_id, &entry, 0);
            self.update_device_statuses();
            EngineResponse::Ok
        } else {
            EngineResponse::Error(format!("{}: not in registry", name))
        }
    }

    fn cmd_stop_device(&mut self, name: &str) -> EngineResponse {
        let source_id = self
            .running
            .iter()
            .find(|(_, p)| p.entry_name == name)
            .map(|(s, _)| s.clone());

        if let Some(source_id) = source_id {
            self.terminate_process(&source_id);
            self.update_device_statuses();
            EngineResponse::Ok
        } else {
            EngineResponse::Error(format!("{}: no running process", name))
        }
    }

    fn cmd_start_device(&mut self, name: &str) -> EngineResponse {
        let source_id = self
            .connected_devices
            .iter()
            .find(|(_, n)| *n == name)
            .map(|(s, _)| s.clone());

        let Some(source_id) = source_id else {
            return EngineResponse::Error(format!("{}: not connected", name));
        };

        if self.running.contains_key(&source_id) {
            return EngineResponse::Error(format!("{}: already running", name));
        }

        let entry = self.registry.devices.iter().find(|e| e.name == name).cloned();
        if let Some(entry) = entry {
            self.spawn_attach_with_count(&source_id, &entry, 0);
            self.update_device_statuses();
            EngineResponse::Ok
        } else {
            EngineResponse::Error(format!("{}: not in registry", name))
        }
    }

    fn cmd_add_device(&mut self, entry: DeviceEntry) -> EngineResponse {
        if self.registry.devices.iter().any(|e| e.name == entry.name) {
            return EngineResponse::Error(format!("'{}' already exists", entry.name));
        }
        self.registry.devices.push(entry);
        match registry::save_registry(&self.registry) {
            Ok(()) => EngineResponse::Ok,
            Err(e) => EngineResponse::Error(e),
        }
    }

    fn cmd_remove_device(&mut self, name: &str) -> EngineResponse {
        let before = self.registry.devices.len();
        self.registry.devices.retain(|e| e.name != name);
        if self.registry.devices.len() == before {
            return EngineResponse::Error(format!("'{}' not found", name));
        }
        match registry::save_registry(&self.registry) {
            Ok(()) => EngineResponse::Ok,
            Err(e) => EngineResponse::Error(e),
        }
    }

    // ── Recording commands ──────────────────────────────────────────

    fn cmd_start_recording(
        &mut self,
        session_id: Option<String>,
        devices: Option<Vec<String>>,
        output_dir: Option<String>,
    ) -> EngineResponse {
        if self.recording.as_ref().is_some_and(|r| {
            matches!(
                r.state,
                RecordingState::Starting | RecordingState::Recording
            )
        }) {
            return EngineResponse::Error("recording already in progress".into());
        }

        let session_id = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let output_dir = output_dir.unwrap_or_else(|| format!("/tmp/rapid_recording/{}", session_id));

        // Determine which devices to record
        let target_devices: Vec<String> = devices.unwrap_or_else(|| {
            self.devices
                .iter()
                .filter(|d| d.online && d.process_running)
                .map(|d| d.name.clone())
                .collect()
        });

        // Verify all target devices are online
        let online_names: HashSet<&str> = self
            .devices
            .iter()
            .filter(|d| d.online && d.process_running)
            .map(|d| d.name.as_str())
            .collect();

        for name in &target_devices {
            if !online_names.contains(name.as_str()) {
                return EngineResponse::Error(format!("device '{}' is not online", name));
            }
        }

        let mut session = RecordingSession::new(session_id, output_dir);

        for name in &target_devices {
            if let Some(entry) = self.registry.devices.iter().find(|e| e.name == *name) {
                let addr = self.device_addresses.get(name.as_str()).map(|s| s.as_str());
                session.start_device(name, entry, addr);
            }
        }

        self.recording = Some(session);
        EngineResponse::Ok
    }

    fn cmd_stop_recording(&mut self, _session_id: Option<String>) -> EngineResponse {
        let Some(ref mut session) = self.recording else {
            return EngineResponse::Error("no active recording".into());
        };

        if !matches!(
            session.state,
            RecordingState::Recording | RecordingState::Starting
        ) {
            return EngineResponse::Error(format!(
                "cannot stop recording in state '{}'",
                session.state.as_str()
            ));
        }

        session.state = RecordingState::Stopping;
        session.device_states.clear();

        let device_names: Vec<String> = self
            .devices
            .iter()
            .filter(|d| d.online && d.process_running)
            .map(|d| d.name.clone())
            .collect();

        for name in &device_names {
            if let Some(entry) = self.registry.devices.iter().find(|e| e.name == *name) {
                let addr = self.device_addresses.get(name.as_str()).map(|s| s.as_str());
                session.stop_device(name, entry, addr);
            }
        }

        EngineResponse::Ok
    }

    fn check_recording(&mut self) {
        if let Some(ref mut session) = self.recording {
            session.check_progress();
        }
    }

    // ── Discovery & device lifecycle ────────────────────────────────

    fn enumerate_and_start(&mut self) {
        let mut all_discovered = Vec::new();
        for source in &mut self.discovery_sources {
            all_discovered.extend(source.enumerate());
        }

        for discovered in all_discovered {
            self.connected_devices
                .insert(discovered.source_id.clone(), discovered.device_name.clone());
            if let Some(addr) = &discovered.address {
                self.device_addresses
                    .insert(discovered.device_name.clone(), addr.clone());
            }
            self.spawn_attach(&discovered.source_id, &discovered.entry);
        }

        self.update_device_statuses();
        self.publish_mask();
    }

    fn poll_discovery(&mut self) {
        let mut all_events = Vec::new();
        for source in &mut self.discovery_sources {
            all_events.extend(source.poll());
        }

        for event in all_events {
            match event {
                DiscoveryEvent::Added(discovered) => {
                    self.connected_devices
                        .insert(discovered.source_id.clone(), discovered.device_name.clone());
                    if let Some(addr) = &discovered.address {
                        self.device_addresses
                            .insert(discovered.device_name.clone(), addr.clone());
                    }
                    self.spawn_attach(&discovered.source_id, &discovered.entry);
                }
                DiscoveryEvent::Removed { source_id } => {
                    if let Some(name) = self.connected_devices.remove(&source_id) {
                        self.device_addresses.remove(&name);
                    }
                    if self.running.contains_key(&source_id) {
                        self.terminate_process(&source_id);
                    }
                }
            }
        }

        self.update_device_statuses();
    }

    fn spawn_attach(&mut self, source_id: &str, entry: &DeviceEntry) {
        self.spawn_attach_with_count(source_id, entry, 0);
    }

    fn spawn_attach_with_count(
        &mut self,
        source_id: &str,
        entry: &DeviceEntry,
        restart_count: u32,
    ) {
        if self.running.contains_key(source_id) || entry.on_attach.is_empty() {
            return;
        }

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&entry.on_attach).process_group(0);

        // Inject DEVICE_ADDR if available
        if let Some(addr) = self.device_addresses.get(&entry.name) {
            cmd.env("DEVICE_ADDR", addr);
        }
        cmd.env("DEVICE_NAME", &entry.name);

        let log_path = self.log_dir.join(format!("{}.log", entry.name));
        if let Ok(log_file) = File::create(&log_path) {
            let log_file_err = log_file
                .try_clone()
                .unwrap_or_else(|_| File::create("/dev/null").unwrap());
            cmd.stdout(Stdio::from(log_file));
            cmd.stderr(Stdio::from(log_file_err));
        }

        if let Ok(child) = cmd.spawn() {
            self.running.insert(
                source_id.to_string(),
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

    fn terminate_process(&mut self, source_id: &str) {
        let Some(mut proc) = self.running.remove(source_id) else {
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

    fn kill_all(&mut self) {
        let source_ids: Vec<String> = self.running.keys().cloned().collect();
        for source_id in source_ids {
            self.terminate_process(&source_id);
        }
    }

    fn shutdown_discovery(&mut self) {
        for source in &mut self.discovery_sources {
            source.shutdown();
        }
    }

    fn reap_exited(&mut self) {
        let mut exited = Vec::new();

        for (source_id, proc) in self.running.iter_mut() {
            if let Ok(Some(_status)) = proc.child.try_wait() {
                let still_exists = self.connected_devices.contains_key(source_id);
                if still_exists {
                    let current_count = if proc.last_restart.elapsed()
                        > Duration::from_secs(RESTART_WINDOW_SECS)
                    {
                        0
                    } else {
                        proc.restart_count
                    };

                    if current_count < MAX_RESTART_COUNT {
                        self.pending_restarts.push(PendingRestart {
                            source_id: source_id.clone(),
                            entry: proc.entry.clone(),
                            restart_count: current_count + 1,
                            restart_at: Instant::now()
                                + Duration::from_millis(RESTART_DELAY_MS),
                        });
                    }
                }
                exited.push(source_id.clone());
            }
        }

        for source_id in &exited {
            self.running.remove(source_id);
        }

        if !exited.is_empty() {
            self.update_device_statuses();
        }
    }

    fn process_pending_restarts(&mut self) {
        let now = Instant::now();
        let mut still_pending = Vec::new();
        let mut ready = Vec::new();
        for r in self.pending_restarts.drain(..) {
            if now >= r.restart_at {
                ready.push(r);
            } else {
                still_pending.push(r);
            }
        }
        self.pending_restarts = still_pending;

        for restart in ready {
            if self.connected_devices.contains_key(&restart.source_id) {
                self.spawn_attach_with_count(
                    &restart.source_id,
                    &restart.entry,
                    restart.restart_count,
                );
                self.update_device_statuses();
            }
        }
    }

    // ── Status & mask ───────────────────────────────────────────────

    fn update_device_statuses(&mut self) {
        let connected_names: HashSet<&String> = self.connected_devices.values().collect();
        let running_pids: HashMap<&String, u32> = self
            .running
            .values()
            .map(|p| (&p.entry_name, p.child.id()))
            .collect();

        for device in &mut self.devices {
            device.online = connected_names.contains(&device.name);
            device.process_running = running_pids.contains_key(&device.name);
            device.pid = running_pids.get(&device.name).copied();
            device.address = self.device_addresses.get(&device.name).cloned();
        }
    }

    fn compute_mask(&self) -> u64 {
        let mut mask: u64 = 0;
        for device in &self.devices {
            if device.online && device.process_running && device.bit_index < 64 {
                mask |= 1u64 << device.bit_index;
            }
        }
        mask
    }

    fn publish_mask(&mut self) {
        self.current_mask = self.compute_mask();

        if let Some(ref mut publisher) = self.mask_publisher {
            let device_count = self.devices.len() as u8;
            if publisher.publish(self.current_mask, device_count).is_ok() {
                self.sequence = publisher.sequence();
                self.publish_count += 1;
            }
        }

        let elapsed = self.last_freq_check.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.measured_freq = self.publish_count as f64 / elapsed.as_secs_f64();
            self.publish_count = 0;
            self.last_freq_check = Instant::now();
        }
    }

    fn publish_debug(&mut self) {
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
                    online: device.online && device.process_running,
                    usb_connected: device.online,
                    process_running: device.process_running,
                    pid: device.pid,
                },
            );
        }

        let _ = debug_pub.publish(&state);
    }

    fn rebuild_devices(&mut self) {
        let connected_names: HashSet<&String> = self.connected_devices.values().collect();
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
            .map(|(bit_index, name)| {
                let backend = self
                    .registry
                    .devices
                    .iter()
                    .find(|e| e.name == *name)
                    .map(|e| e.backend.clone())
                    .unwrap_or_else(|| "usb".to_string());
                DeviceStatus {
                    name: name.clone(),
                    bit_index,
                    online: connected_names.contains(name),
                    process_running: running_pids.contains_key(name),
                    pid: running_pids.get(name).copied(),
                    backend,
                    address: self.device_addresses.get(name).cloned(),
                }
            })
            .collect();
    }
}
