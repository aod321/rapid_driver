//! Engine command/response protocol and handle.

use std::collections::HashMap;
use std::sync::mpsc::{self, SyncSender};

use serde::{Deserialize, Serialize};

use crate::registry::{DeviceEntry, Registry};

/// A single recording file entry returned by list recordings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingFileEntry {
    pub session_id: String,
    pub filename: String,
    pub size_bytes: u64,
    pub created_at: String,
    pub duration_secs: Option<f64>,
    pub message_count: Option<u64>,
    /// True if this file is currently being recorded or replayed (cannot be deleted).
    pub is_locked: bool,
}

/// A failed batch-delete entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchDeleteFailure {
    pub session_id: String,
    pub error: String,
}

/// Replay state enum with richer states than just active/inactive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReplayState {
    Idle,
    Starting,
    Running,
    Stopping,
    Completed,
    Error,
}

/// Replay status snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayStatusInfo {
    pub active: bool,
    pub state: ReplayState,
    pub session_id: Option<String>,
    pub progress: f64,
    pub elapsed_secs: f64,
    pub total_secs: f64,
    pub speed: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Status of a single device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub name: String,
    pub bit_index: usize,
    pub discovered: bool,
    pub process_running: bool,
    pub pid: Option<u32>,
    pub backend: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat_ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_age_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// Full engine status snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineStatus {
    pub devices: Vec<DeviceStatus>,
    pub current_mask: u64,
    pub mask_hex: String,
    pub sequence: u64,
    pub measured_freq: f64,
    pub mask_enabled: bool,
    pub recording: Option<RecordingInfo>,
}

/// Recording session info (serializable).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingInfo {
    pub session_id: String,
    pub state: String,
    pub devices: HashMap<String, String>,
    pub output_dir: String,
    pub elapsed_secs: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcap_message_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcap_file: Option<String>,
}

/// Commands sent to the engine.
pub enum EngineCommand {
    GetStatus,
    GetDevices,
    GetDevice {
        name: String,
    },
    RestartDevice {
        name: String,
    },
    StopDevice {
        name: String,
    },
    StartDevice {
        name: String,
    },
    GetRegistry,
    AddDevice {
        entry: DeviceEntry,
    },
    RemoveDevice {
        name: String,
    },
    GetMask,
    StartRecording {
        session_id: Option<String>,
        devices: Option<Vec<String>>,
        output_dir: Option<String>,
    },
    StopRecording {
        session_id: Option<String>,
    },
    GetRecordingStatus,
    /// Layout operations
    MoveDevice {
        from: usize,
        to: usize,
    },
    SaveLayout,
    ExportLayout {
        path: String,
    },
    ImportLayout {
        path: String,
    },
    /// Recordings management
    ListRecordings {
        sort: Option<String>,
        order: Option<String>,
    },
    DeleteRecording {
        session_id: String,
    },
    DeleteRecordingsBatch {
        session_ids: Vec<String>,
    },
    /// Replay
    StartReplay {
        session_id: String,
        speed: Option<f64>,
    },
    StopReplay,
    GetReplayStatus,
    /// Graceful shutdown.
    Shutdown,
}

/// Responses from the engine.
pub enum EngineResponse {
    Status(EngineStatus),
    Devices(Vec<DeviceStatus>),
    Device(Option<DeviceStatus>),
    Registry(Registry),
    Mask {
        value: u64,
        hex: String,
        layout: Vec<String>,
    },
    RecordingStatus(Option<RecordingInfo>),
    /// Recordings list
    RecordingsList {
        recordings: Vec<RecordingFileEntry>,
        total_size_bytes: u64,
        disk_free_bytes: u64,
    },
    /// Single delete result
    Deleted(String),
    /// Batch delete result
    BatchDeleteResult {
        deleted: Vec<String>,
        failed: Vec<BatchDeleteFailure>,
    },
    /// Replay started
    ReplayStarted {
        session_id: String,
        total_secs: f64,
        message_count: u64,
    },
    /// Replay status
    ReplayStatus(ReplayStatusInfo),
    Ok,
    /// Error with HTTP status code hint
    ErrorWithStatus {
        status: u16,
        message: String,
    },
    Error(String),
}

type CommandMsg = (EngineCommand, Option<SyncSender<EngineResponse>>);

/// Cloneable handle for sending commands to the Engine.
#[derive(Clone)]
pub struct EngineHandle {
    tx: SyncSender<CommandMsg>,
}

impl EngineHandle {
    pub fn new(tx: SyncSender<CommandMsg>) -> Self {
        Self { tx }
    }

    /// Send a command and wait for a response.
    pub fn request(&self, cmd: EngineCommand) -> Result<EngineResponse, String> {
        let (resp_tx, resp_rx) = mpsc::sync_channel(1);
        self.tx
            .send((cmd, Some(resp_tx)))
            .map_err(|_| "engine channel closed".to_string())?;
        resp_rx
            .recv()
            .map_err(|_| "engine response channel closed".to_string())
    }

    /// Fire-and-forget command.
    pub fn send(&self, cmd: EngineCommand) -> Result<(), String> {
        self.tx
            .send((cmd, None))
            .map_err(|_| "engine channel closed".to_string())
    }
}
