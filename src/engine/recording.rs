//! Recording session management.

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use crate::registry::DeviceEntry;

/// Recording session state machine.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordingState {
    Starting,
    Recording,
    Stopping,
    Stopped,
    Failed(String),
}

impl RecordingState {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Starting => "starting",
            Self::Recording => "recording",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed(_) => "failed",
        }
    }
}

/// Per-device recording command state.
pub enum DeviceRecordState {
    Pending,
    Running(Child),
    Done,
    Failed(String),
}

impl DeviceRecordState {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Pending => "pending",
            Self::Running(_) => "running",
            Self::Done => "done",
            Self::Failed(_) => "failed",
        }
    }
}

/// A recording session tracking multi-device synchronized recording.
pub struct RecordingSession {
    pub session_id: String,
    pub state: RecordingState,
    pub output_dir: String,
    pub created_at: Instant,
    pub device_states: HashMap<String, DeviceRecordState>,
}

impl RecordingSession {
    pub fn new(session_id: String, output_dir: String) -> Self {
        Self {
            session_id,
            state: RecordingState::Starting,
            output_dir,
            created_at: Instant::now(),
            device_states: HashMap::new(),
        }
    }

    /// Spawn on_record_start for a device.
    pub fn start_device(&mut self, device_name: &str, entry: &DeviceEntry, address: Option<&str>) {
        if entry.on_record_start.is_empty() {
            self.device_states
                .insert(device_name.to_string(), DeviceRecordState::Done);
            return;
        }

        match self.spawn_cmd(&entry.on_record_start, device_name, address) {
            Ok(child) => {
                self.device_states
                    .insert(device_name.to_string(), DeviceRecordState::Running(child));
            }
            Err(e) => {
                self.device_states
                    .insert(device_name.to_string(), DeviceRecordState::Failed(e));
            }
        }
    }

    /// Spawn on_record_stop for a device.
    pub fn stop_device(&mut self, device_name: &str, entry: &DeviceEntry, address: Option<&str>) {
        if entry.on_record_stop.is_empty() {
            self.device_states
                .insert(device_name.to_string(), DeviceRecordState::Done);
            return;
        }

        match self.spawn_cmd(&entry.on_record_stop, device_name, address) {
            Ok(child) => {
                self.device_states
                    .insert(device_name.to_string(), DeviceRecordState::Running(child));
            }
            Err(e) => {
                self.device_states
                    .insert(device_name.to_string(), DeviceRecordState::Failed(e));
            }
        }
    }

    fn spawn_cmd(
        &self,
        shell_cmd: &str,
        device_name: &str,
        address: Option<&str>,
    ) -> Result<Child, String> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(shell_cmd);
        cmd.env("SESSION_ID", &self.session_id);
        cmd.env("OUTPUT_DIR", &self.output_dir);
        cmd.env("DEVICE_NAME", device_name);
        if let Some(addr) = address {
            cmd.env("DEVICE_ADDR", addr);
        }
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        cmd.spawn().map_err(|e| e.to_string())
    }

    /// Check all running children and advance state machine.
    /// Returns true if state changed.
    pub fn check_progress(&mut self) -> bool {
        let mut changed = false;

        for state in self.device_states.values_mut() {
            if let DeviceRecordState::Running(child) = state {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        if status.success() {
                            *state = DeviceRecordState::Done;
                        } else {
                            *state = DeviceRecordState::Failed(format!(
                                "exit code {}",
                                status.code().unwrap_or(-1)
                            ));
                        }
                        changed = true;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        *state = DeviceRecordState::Failed(e.to_string());
                        changed = true;
                    }
                }
            }
        }

        if !changed {
            return false;
        }

        let all_done = self
            .device_states
            .values()
            .all(|s| matches!(s, DeviceRecordState::Done | DeviceRecordState::Failed(_)));

        let any_failed = self
            .device_states
            .values()
            .any(|s| matches!(s, DeviceRecordState::Failed(_)));

        if all_done {
            match self.state {
                RecordingState::Starting => {
                    if any_failed {
                        self.state = RecordingState::Failed("some devices failed to start".into());
                    } else {
                        self.state = RecordingState::Recording;
                    }
                }
                RecordingState::Stopping => {
                    self.state = RecordingState::Stopped;
                }
                _ => {}
            }
        }

        true
    }

    /// Get serializable device states map.
    pub fn device_state_strings(&self) -> HashMap<String, String> {
        self.device_states
            .iter()
            .map(|(name, state)| (name.clone(), state.as_str().to_string()))
            .collect()
    }
}
