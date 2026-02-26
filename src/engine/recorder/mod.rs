//! Centralized MCAP recorder — subscribes to ZMQ PUB sensors and writes MCAP.

pub mod mcap_writer;
pub mod messages;
pub mod schemas;
pub mod zmq_sub;

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};

use mcap_writer::McapSession;
use messages::SensorMessage;

/// Info about a sensor to subscribe to during recording.
pub struct SensorInfo {
    pub device_name: String,
    pub address: String,
    pub sensor_type: String,
}

/// Snapshot of the hardware mask state for recording.
pub struct MaskSnapshot {
    pub mask: u64,
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub device_count: u8,
    pub devices: HashMap<String, bool>,
}

/// Summary returned after stopping a recording.
pub struct RecordingSummary {
    pub session_id: String,
    pub output_path: String,
    pub message_count: u64,
    pub duration_secs: f64,
}

/// Stats about a currently active recording.
pub struct RecordingStats {
    pub message_count: u64,
    pub duration_secs: f64,
    pub output_path: String,
}

/// Core recorder that manages ZMQ subscriptions and MCAP writing.
pub struct RecorderCore {
    rt: tokio::runtime::Runtime,
    msg_tx: Sender<SensorMessage>,
    msg_rx: Receiver<SensorMessage>,
    active: Option<McapSession>,
    zmq_tasks: Vec<tokio::task::JoinHandle<()>>,
    mask_decimation: u32,
    mask_counter: u32,
}

impl RecorderCore {
    pub fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("recorder")
            .build()
            .expect("failed to build recorder tokio runtime");

        let (msg_tx, msg_rx) = mpsc::channel();

        RecorderCore {
            rt,
            msg_tx,
            msg_rx,
            active: None,
            zmq_tasks: Vec::new(),
            mask_decimation: 50, // 500Hz / 50 = 10Hz mask writes
            mask_counter: 0,
        }
    }

    /// Start a new MCAP recording session.
    pub fn start_recording(
        &mut self,
        session_id: &str,
        output_dir: &str,
        sensors: Vec<SensorInfo>,
    ) -> Result<(), String> {
        if self.active.is_some() {
            return Err("MCAP recording already active".into());
        }

        // Create MCAP session
        let mut session = McapSession::new(session_id, output_dir)?;
        session.register_schemas()?;

        // Spawn ZMQ subscriber tasks
        for sensor in &sensors {
            let tx = self.msg_tx.clone();
            let addr = sensor.address.clone();
            let name = sensor.device_name.clone();

            let handle = self.rt.spawn(zmq_sub::zmq_subscribe_task(addr, name, tx));
            self.zmq_tasks.push(handle);
        }

        eprintln!(
            "[recorder] started session '{}' with {} sensors → {}",
            session_id,
            sensors.len(),
            session.output_path.display(),
        );

        self.active = Some(session);
        self.mask_counter = 0;
        Ok(())
    }

    /// Stop the active recording. Returns summary on success.
    pub fn stop_recording(&mut self) -> Result<RecordingSummary, String> {
        // Abort all ZMQ tasks
        for handle in self.zmq_tasks.drain(..) {
            handle.abort();
        }

        // Drain remaining messages
        while let Ok(msg) = self.msg_rx.try_recv() {
            if let Some(ref mut session) = self.active {
                let _ = session.write_message(&msg);
            }
        }

        let session = self.active.take()
            .ok_or("no active MCAP recording")?;

        let message_count = session.message_count;
        let duration_secs = session.started_at.elapsed().as_secs_f64();
        let session_id = session.session_id.clone();

        let output_path = session.finish()?;

        eprintln!(
            "[recorder] stopped session '{}': {} messages in {:.1}s → {}",
            session_id, message_count, duration_secs, output_path.display(),
        );

        Ok(RecordingSummary {
            session_id,
            output_path: output_path.to_string_lossy().to_string(),
            message_count,
            duration_secs,
        })
    }

    /// Called every engine tick (~500Hz). Drains message channel and writes to MCAP.
    pub fn tick(&mut self, mask: Option<MaskSnapshot>) {
        let Some(ref mut session) = self.active else {
            return;
        };

        // Drain all pending messages
        while let Ok(msg) = self.msg_rx.try_recv() {
            if let Err(e) = session.write_message(&msg) {
                eprintln!("[recorder] write error: {}", e);
            }
        }

        // Write hardware mask at decimated rate (10Hz)
        if let Some(mask) = mask {
            self.mask_counter += 1;
            if self.mask_counter >= self.mask_decimation {
                self.mask_counter = 0;

                let mask_json = serde_json::json!({
                    "type": "hardware_mask",
                    "ts": mask.timestamp_ns as f64 / 1e9,
                    "mask": mask.mask,
                    "sequence": mask.sequence,
                    "device_count": mask.device_count,
                    "devices": mask.devices,
                });

                if let Ok(bytes) = serde_json::to_vec(&mask_json) {
                    if let Err(e) = session.write_mask(&bytes, mask.timestamp_ns) {
                        eprintln!("[recorder] mask write error: {}", e);
                    }
                }
            }
        }
    }

    /// Check if a recording is currently active.
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Get stats about the current recording.
    pub fn stats(&self) -> Option<RecordingStats> {
        self.active.as_ref().map(|s| RecordingStats {
            message_count: s.message_count,
            duration_secs: s.started_at.elapsed().as_secs_f64(),
            output_path: s.output_path.to_string_lossy().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_recorder_full_cycle() {
        let dir = std::env::temp_dir().join("rapid_test_recorder");
        let _ = std::fs::remove_dir_all(&dir);
        let dir_str = dir.to_str().unwrap();

        let mut recorder = RecorderCore::new();

        // Start with no sensors (just mask data)
        recorder
            .start_recording("test_full", dir_str, vec![])
            .expect("start failed");
        assert!(recorder.is_active());

        // Simulate a few ticks with mask data
        for i in 0..100 {
            let snap = MaskSnapshot {
                mask: 0x01,
                sequence: i,
                timestamp_ns: 1_000_000_000 + i * 2_000_000,
                device_count: 1,
                devices: HashMap::from([("dev1".into(), true)]),
            };
            recorder.tick(Some(snap));
        }

        let stats = recorder.stats().expect("stats should exist");
        println!("Messages before stop: {}", stats.message_count);

        // Stop
        let summary = recorder.stop_recording().expect("stop failed");
        println!(
            "Summary: {} messages, {:.1}s, path={}",
            summary.message_count, summary.duration_secs, summary.output_path
        );

        assert!(!recorder.is_active());
        let path = std::path::Path::new(&summary.output_path);
        assert!(path.exists(), "mcap file missing: {}", summary.output_path);
        let size = std::fs::metadata(path).unwrap().len();
        println!("File size: {} bytes", size);
        assert!(size > 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
