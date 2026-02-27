//! Engine command/response protocol and handle.

use std::collections::HashMap;
use std::sync::mpsc::{self, SyncSender};

use serde::{Deserialize, Serialize};

use crate::registry::{DeviceEntry, Registry};

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
    Ok,
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
