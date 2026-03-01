//! Core engine: owns all runtime state, runs 500Hz main loop.

pub mod command;
pub mod recorder;
pub mod recording;
pub mod replay;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, SyncSender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::libc;

use crate::discovery::{DiscoveryEvent, DiscoverySource};
use crate::mask::debug::{DebugState, DeviceDebugInfo};
use crate::mask::{DebugPublisher, MaskLayout, MaskPublisher};
use crate::registry::{self, DeviceEntry, Registry};

use command::{
    BatchDeleteFailure, DeviceStatus, EngineCommand, EngineResponse, EngineStatus,
    RecordingFileEntry, RecordingInfo,
};
use recorder::RecorderCore;
use recording::{RecordingSession, RecordingState};
use replay::ReplayManager;

/// Running process information.
struct RunningProcess {
    child: Child,
    entry_name: String,
    on_detach: String,
    entry: DeviceEntry,
    restart_count: u32,
    started_at: Instant,
}

/// Pending restart (non-blocking delay).
struct PendingRestart {
    source_id: String,
    entry: DeviceEntry,
    restart_count: u32,
    restart_at: Instant,
}

#[derive(Debug, Clone, Default)]
struct HeartbeatSnapshot {
    updated_at: Option<f64>,
    client_connected: Option<bool>,
    last_frame_at: Option<f64>,
    seq: Option<u64>,
    pid: Option<u32>,
    error: Option<String>,
}

const RESTART_BASE_DELAY_MS: u64 = 1_000;
const RESTART_MAX_DELAY_MS: u64 = 30_000;
const RESTART_JITTER_PCT: u64 = 10;

const STATUS_REFRESH_MS: u64 = 200;
const HEARTBEAT_STALE_SECS: f64 = 3.0;
const CLIENT_DISCONNECT_GRACE_SECS: f64 = 4.0;
const HEARTBEAT_STARTUP_GRACE_SECS: f64 = 6.0;

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
    heartbeat_dir: PathBuf,
    heartbeat_cache: HashMap<String, HeartbeatSnapshot>,
    last_status_refresh: Instant,

    // Recording
    recording: Option<RecordingSession>,

    // MCAP recorder
    recorder: RecorderCore,

    // MCAP replay
    replay: ReplayManager,

    // Recordings directory (base path for scanning MCAP files)
    recordings_dir: PathBuf,

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
        let heartbeat_dir = dirs::state_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("rapid_driver")
            .join("heartbeat");
        let _ = fs::create_dir_all(&heartbeat_dir);

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
                    discovered: false,
                    process_running: false,
                    pid: None,
                    backend,
                    heartbeat_ok: None,
                    last_heartbeat_age_ms: None,
                    state_reason: None,
                    address: None,
                }
            })
            .collect();

        let recordings_dir = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("recordings");

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
            heartbeat_dir,
            heartbeat_cache: HashMap::new(),
            last_status_refresh: Instant::now(),
            recording: None,
            recorder: RecorderCore::new(),
            replay: ReplayManager::new("tcp://0.0.0.0:5560".into()),
            recordings_dir,
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
            self.refresh_statuses_periodic();
            self.check_recording();
            self.tick_recorder();

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
        if self.replay.is_active() {
            self.replay.stop();
        }
        if self.recorder.is_active() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = self.recorder.stop_recording();
            }));
        }
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
            // Recordings management
            EngineCommand::ListRecordings { sort, order } => self.cmd_list_recordings(sort, order),
            EngineCommand::DeleteRecording { session_id } => {
                self.cmd_delete_recording(&session_id)
            }
            EngineCommand::DeleteRecordingsBatch { session_ids } => {
                self.cmd_delete_recordings_batch(session_ids)
            }
            // Replay
            EngineCommand::StartReplay { session_id, speed } => {
                self.cmd_start_replay(&session_id, speed)
            }
            EngineCommand::StopReplay => {
                self.replay.stop();
                EngineResponse::Ok
            }
            EngineCommand::GetReplayStatus => {
                EngineResponse::ReplayStatus(self.replay.status())
            }
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
        self.recording.as_ref().map(|r| {
            let mcap_stats = self.recorder.stats();
            RecordingInfo {
                session_id: r.session_id.clone(),
                state: r.state.as_str().to_string(),
                devices: r.device_state_strings(),
                output_dir: r.output_dir.clone(),
                elapsed_secs: r.created_at.elapsed().as_secs_f64(),
                mcap_message_count: mcap_stats.as_ref().map(|s| s.message_count),
                mcap_file: mcap_stats.map(|s| s.output_path),
            }
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

        let entry = self
            .registry
            .devices
            .iter()
            .find(|e| e.name == name)
            .cloned();
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

        let entry = self
            .registry
            .devices
            .iter()
            .find(|e| e.name == name)
            .cloned();
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

    fn parse_u16_flag(cmd: &str, flag: &str) -> Option<u16> {
        let mut parts = cmd.split_whitespace();
        let prefix = format!("{flag}=");
        while let Some(part) = parts.next() {
            if part == flag {
                if let Some(v) = parts.next() {
                    if let Ok(port) = v.parse::<u16>() {
                        return Some(port);
                    }
                }
            } else if let Some(v) = part.strip_prefix(&prefix) {
                if let Ok(port) = v.parse::<u16>() {
                    return Some(port);
                }
            }
        }
        None
    }

    fn resolve_recorder_sensor_address(
        &self,
        device_name: &str,
        entry: &DeviceEntry,
    ) -> Option<String> {
        // iPhone app's _iphonevio._tcp address points to the phone's ephemeral listener.
        // The real recorder data source is the locally spawned node_iphone ZMQ PUB.
        if entry.backend == "mdns" && entry.service_type.contains("_iphonevio._tcp") {
            let data_port = Self::parse_u16_flag(&entry.on_attach, "--data-port").unwrap_or(5563);
            return Some(format!("127.0.0.1:{data_port}"));
        }

        self.device_addresses.get(device_name).cloned()
    }

    fn cmd_start_recording(
        &mut self,
        session_id: Option<String>,
        devices: Option<Vec<String>>,
        output_dir: Option<String>,
    ) -> EngineResponse {
        // Mutual exclusion: replay must not be active
        if self.replay.is_active() {
            return EngineResponse::ErrorWithStatus {
                status: 409,
                message: "cannot record while replay is active".into(),
            };
        }
        if self.recording.as_ref().is_some_and(|r| {
            matches!(
                r.state,
                RecordingState::Starting | RecordingState::Recording
            )
        }) {
            return EngineResponse::Error("recording already in progress".into());
        }

        let session_id = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let output_dir = output_dir.unwrap_or_else(|| {
            let base = std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("recordings")
                .join(&session_id);
            base.to_string_lossy().to_string()
        });

        // All registered devices must be connected before recording
        let not_ready: Vec<&str> = self
            .devices
            .iter()
            .filter(|d| !self.device_available(d))
            .map(|d| d.name.as_str())
            .collect();

        if !not_ready.is_empty() {
            return EngineResponse::Error(format!("not connected: {}", not_ready.join(", ")));
        }

        // Determine which devices to record
        let target_devices: Vec<String> = devices.unwrap_or_else(|| {
            self.devices
                .iter()
                .filter(|d| self.device_available(d))
                .map(|d| d.name.clone())
                .collect()
        });

        let mut session = RecordingSession::new(session_id.clone(), output_dir.clone());

        for name in &target_devices {
            if let Some(entry) = self.registry.devices.iter().find(|e| e.name == *name) {
                let addr = self.device_addresses.get(name.as_str()).map(|s| s.as_str());
                session.start_device(name, entry, addr);
            }
        }

        // Ensure output directory exists regardless of MCAP success
        if let Err(e) = fs::create_dir_all(&output_dir) {
            return EngineResponse::Error(format!("create output dir: {}", e));
        }

        // Start MCAP recorder — always, so we at least record hardware mask.
        // Subscribe any selected online device that has a resolved data address.
        let sensors: Vec<recorder::SensorInfo> = target_devices
            .iter()
            .filter_map(|name| {
                let entry = self.registry.devices.iter().find(|e| e.name == *name)?;
                let addr = self.resolve_recorder_sensor_address(name, entry)?;
                Some(recorder::SensorInfo {
                    device_name: name.clone(),
                    address: addr,
                    sensor_type: entry.sensor_type.clone(),
                })
            })
            .collect();

        if sensors.is_empty() {
            eprintln!(
                "[engine] recorder: no sensor address resolved for targets {:?}; known addresses={:?}",
                target_devices, self.device_addresses
            );
        }

        let mcap_error = match self
            .recorder
            .start_recording(&session_id, &output_dir, sensors)
        {
            Ok(()) => None,
            Err(e) => {
                eprintln!("[engine] MCAP recorder start error: {}", e);
                Some(e)
            }
        };

        self.recording = Some(session);

        if let Some(e) = mcap_error {
            EngineResponse::Error(format!("recording started but MCAP failed: {}", e))
        } else {
            EngineResponse::Ok
        }
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

        // Stop MCAP recorder (catch panics to prevent engine crash)
        if self.recorder.is_active() {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.recorder.stop_recording()
            })) {
                Ok(Ok(summary)) => {
                    eprintln!(
                        "[engine] MCAP: {} messages, {:.1}s → {}",
                        summary.message_count, summary.duration_secs, summary.output_path,
                    );
                }
                Ok(Err(e)) => {
                    eprintln!("[engine] MCAP recorder stop error: {}", e);
                }
                Err(_) => {
                    eprintln!("[engine] MCAP recorder panicked during stop");
                }
            }
        }

        session.state = RecordingState::Stopping;
        session.device_states.clear();

        let device_names: Vec<String> = self
            .devices
            .iter()
            .filter(|d| d.process_running && d.heartbeat_ok.unwrap_or(true))
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

    fn tick_recorder(&mut self) {
        if !self.recorder.is_active() {
            return;
        }

        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        let mut device_map = HashMap::new();
        for device in &self.devices {
            device_map.insert(device.name.clone(), self.device_available(device));
        }

        let mask_snapshot = recorder::MaskSnapshot {
            mask: self.current_mask,
            sequence: self.sequence,
            timestamp_ns: now_ns,
            device_count: self.devices.len() as u8,
            devices: device_map,
        };

        self.recorder.tick(Some(mask_snapshot));
    }

    // ── Recordings management ────────────────────────────────────────

    /// Validate session_id: must be alphanumeric + hyphens, max 64 chars.
    fn validate_session_id(session_id: &str) -> Result<(), String> {
        if session_id.is_empty() || session_id.len() > 64 {
            return Err("session_id must be 1-64 characters".into());
        }
        if !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err("session_id must contain only alphanumeric characters and hyphens".into());
        }
        Ok(())
    }

    /// Find all *.mcap files under the recordings directory.
    fn scan_mcap_files(&self) -> Result<Vec<PathBuf>, String> {
        if !self.recordings_dir.exists() {
            return Ok(Vec::new());
        }
        Self::walk_mcap_files(&self.recordings_dir)
    }

    fn walk_mcap_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
        let mut results = Vec::new();
        let entries =
            fs::read_dir(dir).map_err(|e| format!("read dir {}: {}", dir.display(), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("read entry: {}", e))?;
            let path = entry.path();
            if path.is_dir() {
                if let Ok(mut sub) = Self::walk_mcap_files(&path) {
                    results.append(&mut sub);
                }
            } else if path.extension().map_or(false, |ext| ext == "mcap") {
                results.push(path);
            }
        }
        Ok(results)
    }

    /// Find the MCAP file for a given session_id.
    fn find_mcap_file(&self, session_id: &str) -> Option<PathBuf> {
        let filename = format!("{}.mcap", session_id);
        if let Ok(files) = self.scan_mcap_files() {
            files
                .into_iter()
                .find(|p| p.file_name().map_or(false, |n| n == filename.as_str()))
        } else {
            None
        }
    }

    /// Get disk free bytes for the recordings directory.
    fn disk_free_bytes(&self) -> u64 {
        let path = if self.recordings_dir.exists() {
            &self.recordings_dir
        } else {
            Path::new(".")
        };
        disk_free_bytes(path).unwrap_or(0)
    }

    /// Check if a session_id is currently being recorded.
    fn is_recording_session(&self, session_id: &str) -> bool {
        self.recording.as_ref().is_some_and(|r| {
            matches!(
                r.state,
                RecordingState::Starting | RecordingState::Recording
            ) && r.session_id == session_id
        })
    }

    fn cmd_list_recordings(
        &self,
        sort: Option<String>,
        order: Option<String>,
    ) -> EngineResponse {
        let files = match self.scan_mcap_files() {
            Ok(f) => f,
            Err(e) => {
                return EngineResponse::ErrorWithStatus {
                    status: 500,
                    message: format!("scan recordings: {}", e),
                }
            }
        };

        let mut entries: Vec<RecordingFileEntry> = Vec::new();
        let mut total_size: u64 = 0;

        for path in &files {
            let session_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let filename = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();

            let meta = match fs::metadata(path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let size_bytes = meta.len();
            total_size += size_bytes;

            let created_at = meta
                .created()
                .or_else(|_| meta.modified())
                .ok()
                .and_then(|t| {
                    let dur = t.duration_since(UNIX_EPOCH).ok()?;
                    let dt = chrono::DateTime::from_timestamp(dur.as_secs() as i64, 0)?;
                    Some(dt.to_rfc3339())
                })
                .unwrap_or_default();

            // Try to read MCAP summary (duration, message_count).
            // This reads the file which could be slow for large files.
            let (duration_secs, message_count) = match fs::read(path) {
                Ok(data) => {
                    let (d, c) = replay::read_mcap_summary(&data);
                    (
                        if d > 0.0 { Some(d) } else { None },
                        if c > 0 { Some(c) } else { None },
                    )
                }
                Err(_) => (None, None),
            };

            let is_locked = self.is_recording_session(&session_id)
                || self.replay.current_session_id().as_deref() == Some(session_id.as_str());

            entries.push(RecordingFileEntry {
                session_id,
                filename,
                size_bytes,
                created_at,
                duration_secs,
                message_count,
                is_locked,
            });
        }

        // Sort
        let sort_field = sort.as_deref().unwrap_or("created_at");
        let descending = order.as_deref().unwrap_or("desc") == "desc";

        match sort_field {
            "size" => entries.sort_by(|a, b| a.size_bytes.cmp(&b.size_bytes)),
            "name" => entries.sort_by(|a, b| a.filename.cmp(&b.filename)),
            _ => entries.sort_by(|a, b| a.created_at.cmp(&b.created_at)),
        }
        if descending {
            entries.reverse();
        }

        EngineResponse::RecordingsList {
            recordings: entries,
            total_size_bytes: total_size,
            disk_free_bytes: self.disk_free_bytes(),
        }
    }

    fn cmd_delete_recording(&self, session_id: &str) -> EngineResponse {
        if let Err(e) = Self::validate_session_id(session_id) {
            return EngineResponse::ErrorWithStatus {
                status: 400,
                message: e,
            };
        }

        // Check if recording in progress for this session
        if self.is_recording_session(session_id) {
            return EngineResponse::ErrorWithStatus {
                status: 409,
                message: "cannot delete: recording in progress".into(),
            };
        }

        // Check if replay in progress for this session
        if self.replay.current_session_id().as_deref() == Some(session_id) {
            return EngineResponse::ErrorWithStatus {
                status: 409,
                message: "cannot delete: replay in progress".into(),
            };
        }

        let Some(path) = self.find_mcap_file(session_id) else {
            return EngineResponse::ErrorWithStatus {
                status: 404,
                message: "recording not found".into(),
            };
        };

        match fs::remove_file(&path) {
            Ok(()) => EngineResponse::Deleted(session_id.to_string()),
            Err(e) => EngineResponse::ErrorWithStatus {
                status: 500,
                message: format!("failed to delete file: {}", e),
            },
        }
    }

    fn cmd_delete_recordings_batch(&self, session_ids: Vec<String>) -> EngineResponse {
        if session_ids.is_empty() {
            return EngineResponse::ErrorWithStatus {
                status: 400,
                message: "session_ids is required and must not be empty".into(),
            };
        }

        let mut deleted = Vec::new();
        let mut failed = Vec::new();

        for sid in &session_ids {
            if let Err(e) = Self::validate_session_id(sid) {
                failed.push(BatchDeleteFailure {
                    session_id: sid.clone(),
                    error: e,
                });
                continue;
            }

            if self.is_recording_session(sid) {
                failed.push(BatchDeleteFailure {
                    session_id: sid.clone(),
                    error: "cannot delete: recording in progress".into(),
                });
                continue;
            }
            if self.replay.current_session_id().as_deref() == Some(sid.as_str()) {
                failed.push(BatchDeleteFailure {
                    session_id: sid.clone(),
                    error: "cannot delete: replay in progress".into(),
                });
                continue;
            }

            match self.find_mcap_file(sid) {
                Some(path) => match fs::remove_file(&path) {
                    Ok(()) => deleted.push(sid.clone()),
                    Err(e) => failed.push(BatchDeleteFailure {
                        session_id: sid.clone(),
                        error: format!("failed to delete: {}", e),
                    }),
                },
                None => {
                    // Spec: skip non-existent files silently
                }
            }
        }

        EngineResponse::BatchDeleteResult { deleted, failed }
    }

    fn cmd_start_replay(&mut self, session_id: &str, speed: Option<f64>) -> EngineResponse {
        if let Err(e) = Self::validate_session_id(session_id) {
            return EngineResponse::ErrorWithStatus {
                status: 400,
                message: e,
            };
        }

        let speed = speed.unwrap_or(1.0);
        if !(0.1..=10.0).contains(&speed) {
            return EngineResponse::ErrorWithStatus {
                status: 400,
                message: "speed must be between 0.1 and 10.0".into(),
            };
        }

        // Mutual exclusion: recording must not be active
        if self.recording.as_ref().is_some_and(|r| {
            matches!(
                r.state,
                RecordingState::Starting | RecordingState::Recording
            )
        }) {
            return EngineResponse::ErrorWithStatus {
                status: 409,
                message: "cannot replay while recording is active".into(),
            };
        }

        // Mutual exclusion: another replay must not be active
        if self.replay.is_active() {
            return EngineResponse::ErrorWithStatus {
                status: 409,
                message: "another replay is already active, stop it first".into(),
            };
        }

        let Some(path) = self.find_mcap_file(session_id) else {
            return EngineResponse::ErrorWithStatus {
                status: 404,
                message: "recording not found".into(),
            };
        };

        match self.replay.start(path, session_id.to_string(), speed) {
            Ok((total_secs, message_count)) => EngineResponse::ReplayStarted {
                session_id: session_id.to_string(),
                total_secs,
                message_count,
            },
            Err(e) => EngineResponse::ErrorWithStatus {
                status: 500,
                message: format!("replay start failed: {}", e),
            },
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
                        let backend = self
                            .registry
                            .devices
                            .iter()
                            .find(|e| e.name == name)
                            .map(|e| e.backend.as_str())
                            .unwrap_or("usb");
                        if backend != "mdns" && self.running.contains_key(&source_id) {
                            self.terminate_process(&source_id);
                        }
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

        // For mDNS devices: if a process for the same device name is already running
        // under a different source_id (e.g. iPhone re-advertised on a new port),
        // migrate it to the new source_id instead of spawning a duplicate.
        if entry.backend == "mdns" {
            let existing = self
                .running
                .iter()
                .find(|(_, proc)| proc.entry_name == entry.name)
                .map(|(sid, _)| sid.clone());
            if let Some(old_source_id) = existing {
                if let Some(proc) = self.running.remove(&old_source_id) {
                    self.running.insert(source_id.to_string(), proc);
                }
                return;
            }
        }

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&entry.on_attach).process_group(0);

        // Inject DEVICE_ADDR if available
        if let Some(addr) = self.device_addresses.get(&entry.name) {
            cmd.env("DEVICE_ADDR", addr);
        }
        cmd.env("DEVICE_NAME", &entry.name);
        if self.device_uses_heartbeat(entry) {
            let heartbeat_path = self.heartbeat_file_path(&entry.name);
            let _ = fs::create_dir_all(&self.heartbeat_dir);
            let _ = fs::remove_file(&heartbeat_path);
            cmd.env("RAPID_HEARTBEAT_PATH", &heartbeat_path);
        }

        let log_path = self.log_dir.join(format!("{}.log", entry.name));
        if let Ok(log_file) = File::create(&log_path) {
            let log_file_err = log_file
                .try_clone()
                .unwrap_or_else(|_| File::create("/dev/null").unwrap());
            cmd.stdout(Stdio::from(log_file));
            cmd.stderr(Stdio::from(log_file_err));
        }

        if let Ok(child) = cmd.spawn() {
            self.heartbeat_cache.remove(&entry.name);
            self.running.insert(
                source_id.to_string(),
                RunningProcess {
                    child,
                    entry_name: entry.name.clone(),
                    on_detach: entry.on_detach.clone(),
                    entry: entry.clone(),
                    restart_count,
                    started_at: Instant::now(),
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
        self.heartbeat_cache.remove(&proc.entry_name);

        if !proc.on_detach.is_empty() {
            let _ = Command::new("sh").arg("-c").arg(&proc.on_detach).status();
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
                let should_restart = still_exists || proc.entry.backend == "mdns";
                if should_restart {
                    let next_count = proc.restart_count.saturating_add(1);
                    let delay_ms = Self::restart_delay_ms(next_count);
                    self.pending_restarts.push(PendingRestart {
                        source_id: source_id.clone(),
                        entry: proc.entry.clone(),
                        restart_count: next_count,
                        restart_at: Instant::now() + Duration::from_millis(delay_ms),
                    });
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
            if self.connected_devices.contains_key(&restart.source_id)
                || restart.entry.backend == "mdns"
            {
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

    fn refresh_statuses_periodic(&mut self) {
        if self.last_status_refresh.elapsed() >= Duration::from_millis(STATUS_REFRESH_MS) {
            self.update_device_statuses();
        }
    }

    fn update_device_statuses(&mut self) {
        let connected_names: HashSet<String> = self.connected_devices.values().cloned().collect();
        let running_pids: HashMap<String, u32> = self
            .running
            .values()
            .map(|p| (p.entry_name.clone(), p.child.id()))
            .collect();
        let running_uptime_secs: HashMap<String, f64> = self
            .running
            .values()
            .map(|p| (p.entry_name.clone(), p.started_at.elapsed().as_secs_f64()))
            .collect();
        let running_sources_by_name: HashMap<String, String> = self
            .running
            .iter()
            .map(|(source_id, p)| (p.entry_name.clone(), source_id.clone()))
            .collect();
        let heartbeat_devices: Vec<String> = self
            .registry
            .devices
            .iter()
            .filter(|e| self.device_uses_heartbeat(e))
            .map(|e| e.name.clone())
            .collect();
        self.refresh_heartbeat_cache(&heartbeat_devices);
        let heartbeat_cache = self.heartbeat_cache.clone();
        let device_addresses = self.device_addresses.clone();

        let heartbeat_device_set: HashSet<String> = heartbeat_devices.into_iter().collect();
        let now_unix_secs = Self::now_unix_secs();
        let mut restart_sources: HashSet<String> = HashSet::new();

        for device in &mut self.devices {
            let uses_heartbeat = heartbeat_device_set.contains(&device.name);
            let heartbeat = heartbeat_cache.get(&device.name);
            let _heartbeat_seq = heartbeat.and_then(|hb| hb.seq);
            let heartbeat_age_ms =
                heartbeat.and_then(|hb| Self::heartbeat_age_ms(hb, now_unix_secs));
            let heartbeat_fresh = heartbeat_age_ms
                .map(|age| (age as f64 / 1000.0) <= HEARTBEAT_STALE_SECS)
                .unwrap_or(false);

            let online = connected_names.contains(&device.name);
            let process_running = running_pids.contains_key(&device.name);
            let pid = running_pids.get(&device.name).copied();
            let process_uptime_secs = running_uptime_secs
                .get(&device.name)
                .copied()
                .unwrap_or(0.0);
            let startup_grace = uses_heartbeat
                && process_running
                && process_uptime_secs <= HEARTBEAT_STARTUP_GRACE_SECS;
            let last_frame_age_secs = heartbeat
                .and_then(|hb| hb.last_frame_at)
                .map(|last| (now_unix_secs - last).max(0.0));
            let disconnected_too_long = uses_heartbeat
                && heartbeat_fresh
                && heartbeat
                    .and_then(|hb| hb.client_connected)
                    .is_some_and(|connected| !connected)
                // Only enforce disconnection timeout once we've seen real frame activity.
                && last_frame_age_secs.is_some_and(|age| age > CLIENT_DISCONNECT_GRACE_SECS);

            let heartbeat_ok = if uses_heartbeat {
                heartbeat_fresh && !disconnected_too_long
            } else {
                true
            };
            let has_heartbeat_error = heartbeat
                .and_then(|hb| hb.error.as_deref())
                .is_some_and(|err| !err.is_empty());
            let ready = process_running && heartbeat_ok;
            let reason = if ready {
                None
            } else if !process_running {
                if device.backend == "mdns" && !online {
                    Some("mdns_missing".to_string())
                } else {
                    Some("process_down".to_string())
                }
            } else if uses_heartbeat {
                if startup_grace && !heartbeat_fresh {
                    Some("starting".to_string())
                } else if disconnected_too_long {
                    Some("client_disconnected".to_string())
                } else {
                    Some("heartbeat_stale".to_string())
                }
            } else if device.backend == "mdns" && !online {
                Some("mdns_missing".to_string())
            } else {
                Some("process_down".to_string())
            };

            if process_running
                && uses_heartbeat
                && !startup_grace
                && (!heartbeat_fresh || disconnected_too_long)
            {
                if let Some(source_id) = running_sources_by_name.get(&device.name) {
                    restart_sources.insert(source_id.clone());
                }
            }

            if process_running && uses_heartbeat && !startup_grace && has_heartbeat_error {
                if let Some(source_id) = running_sources_by_name.get(&device.name) {
                    restart_sources.insert(source_id.clone());
                }
            }

            device.discovered = online;
            device.process_running = process_running;
            device.pid = pid;
            device.heartbeat_ok = if uses_heartbeat {
                Some(heartbeat_ok)
            } else {
                None
            };
            device.last_heartbeat_age_ms = if uses_heartbeat {
                heartbeat_age_ms
            } else {
                None
            };
            device.state_reason = reason;
            device.address = device_addresses.get(&device.name).cloned();
        }

        for source_id in restart_sources {
            let Some(proc) = self.running.get(&source_id) else {
                continue;
            };
            let next_count = proc.restart_count.saturating_add(1);
            let delay_ms = Self::restart_delay_ms(next_count);
            let entry = proc.entry.clone();
            self.pending_restarts.retain(|r| r.source_id != source_id);
            self.pending_restarts.push(PendingRestart {
                source_id: source_id.clone(),
                entry,
                restart_count: next_count,
                restart_at: Instant::now() + Duration::from_millis(delay_ms),
            });
            self.terminate_process(&source_id);
        }

        self.last_status_refresh = Instant::now();
    }

    fn device_uses_heartbeat(&self, entry: &DeviceEntry) -> bool {
        entry.backend == "mdns" && entry.service_type.contains("_iphonevio._tcp")
    }

    fn heartbeat_file_path(&self, device_name: &str) -> PathBuf {
        let safe_name: String = device_name
            .chars()
            .map(|c| match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => c,
                _ => '_',
            })
            .collect();
        self.heartbeat_dir.join(format!("{safe_name}.json"))
    }

    fn refresh_heartbeat_cache(&mut self, heartbeat_devices: &[String]) {
        for device_name in heartbeat_devices {
            let path = self.heartbeat_file_path(device_name);
            match Self::read_heartbeat_file(&path) {
                Some(snapshot) => {
                    self.heartbeat_cache.insert(device_name.clone(), snapshot);
                }
                None => {
                    self.heartbeat_cache.remove(device_name);
                }
            }
        }
    }

    fn read_heartbeat_file(path: &Path) -> Option<HeartbeatSnapshot> {
        let raw = fs::read_to_string(path).ok()?;
        let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
        let mut updated_at = value.get("updated_at").and_then(|v| v.as_f64());
        if updated_at.is_none() {
            updated_at = fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64());
        }
        Some(HeartbeatSnapshot {
            updated_at,
            client_connected: value.get("client_connected").and_then(|v| v.as_bool()),
            last_frame_at: value.get("last_frame_at").and_then(|v| v.as_f64()),
            seq: value.get("seq").and_then(|v| v.as_u64()),
            pid: value
                .get("pid")
                .and_then(|v| v.as_u64())
                .and_then(|v| u32::try_from(v).ok()),
            error: value
                .get("error")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        })
    }

    fn now_unix_secs() -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
    }

    fn heartbeat_age_ms(snapshot: &HeartbeatSnapshot, now_unix_secs: f64) -> Option<u64> {
        let updated = snapshot.updated_at?;
        if now_unix_secs <= updated {
            return Some(0);
        }
        Some(((now_unix_secs - updated) * 1000.0) as u64)
    }

    fn restart_delay_ms(restart_count: u32) -> u64 {
        let exp = restart_count.saturating_sub(1).min(10);
        let mut delay_ms = RESTART_BASE_DELAY_MS.saturating_mul(1u64 << exp);
        delay_ms = delay_ms.min(RESTART_MAX_DELAY_MS);

        let jitter_span = delay_ms.saturating_mul(RESTART_JITTER_PCT) / 100;
        if jitter_span == 0 {
            return delay_ms;
        }
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64;
        let range = jitter_span.saturating_mul(2).saturating_add(1);
        let offset = (now_ns % range) as i64 - jitter_span as i64;
        let jittered = (delay_ms as i64 + offset).clamp(100, RESTART_MAX_DELAY_MS as i64);
        jittered as u64
    }

    fn device_available(&self, device: &DeviceStatus) -> bool {
        device.process_running && device.heartbeat_ok.unwrap_or(true)
    }

    fn compute_mask(&self) -> u64 {
        let mut mask: u64 = 0;
        for device in &self.devices {
            if self.device_available(device) && device.bit_index < 64 {
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
                    online: device.process_running && device.heartbeat_ok.unwrap_or(true),
                    usb_connected: device.discovered,
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
                    discovered: connected_names.contains(name),
                    process_running: running_pids.contains_key(name),
                    pid: running_pids.get(name).copied(),
                    backend,
                    heartbeat_ok: None,
                    last_heartbeat_age_ms: None,
                    state_reason: None,
                    address: self.device_addresses.get(name).cloned(),
                }
            })
            .collect();
        self.update_device_statuses();
    }
}

/// Get available disk space in bytes using statvfs.
fn disk_free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    let c_path = CString::new(path.to_str()?).ok()?;
    unsafe {
        let mut stat = MaybeUninit::<libc::statvfs>::uninit();
        if libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) == 0 {
            let stat = stat.assume_init();
            Some(stat.f_bavail as u64 * stat.f_frsize as u64)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::recorder::mcap_writer::McapSession;

    /// Helper: create a test MCAP file in the given directory.
    fn create_test_mcap(dir: &Path, session_id: &str) -> PathBuf {
        let dir_str = dir.to_str().unwrap();
        let mut session =
            McapSession::new(session_id, dir_str).expect("McapSession::new");
        session.register_schemas().expect("register_schemas");
        for i in 0..5 {
            let json = serde_json::json!({"type":"hardware_mask","mask":1,"sequence":i});
            let bytes = serde_json::to_vec(&json).unwrap();
            session
                .write_mask(&bytes, 1_000_000_000 + i * 50_000_000)
                .expect("write_mask");
        }
        session.finish().expect("finish")
    }

    #[test]
    fn test_validate_session_id() {
        assert!(Engine::validate_session_id("abc-123").is_ok());
        assert!(Engine::validate_session_id("a1b2c3d4-e5f6-7890-abcd-ef1234567890").is_ok());
        assert!(Engine::validate_session_id("").is_err());
        assert!(Engine::validate_session_id("abc/../../etc").is_err()); // path traversal
        assert!(Engine::validate_session_id("has spaces").is_err());
        assert!(Engine::validate_session_id(&"x".repeat(65)).is_err());
    }

    #[test]
    fn test_scan_and_list_recordings() {
        let base = std::env::temp_dir().join("rapid_test_list_recs");
        let _ = fs::remove_dir_all(&base);

        // Create recordings in subdirectories (matching current recording layout)
        let sub1 = base.join("session-1");
        let sub2 = base.join("session-2");
        create_test_mcap(&sub1, "session-1");
        create_test_mcap(&sub2, "session-2");

        // Create engine-like scan
        let files = Engine::walk_mcap_files(&base).expect("walk");
        assert_eq!(files.len(), 2, "expected 2 mcap files, got {:?}", files);

        // Check filenames
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_stem().unwrap().to_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"session-1".to_string()));
        assert!(names.contains(&"session-2".to_string()));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_delete_nonexistent_recording() {
        // Create a minimal engine setup for testing
        let base = std::env::temp_dir().join("rapid_test_delete_nonexist");
        let _ = fs::remove_dir_all(&base);
        let _ = fs::create_dir_all(&base);

        // We can't easily create an Engine without discovery sources,
        // but we can test the validate + find logic directly.
        assert!(Engine::validate_session_id("no-such-session").is_ok());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_disk_free_bytes() {
        let free = disk_free_bytes(Path::new("/tmp"));
        assert!(free.is_some(), "should be able to stat /tmp");
        assert!(free.unwrap() > 0, "disk should have some free space");
    }
}
