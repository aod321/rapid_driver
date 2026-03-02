//! MCAP replay manager — reads MCAP files and publishes messages to ZMQ.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::engine::command::{ReplayState, ReplayStatusInfo};
use crate::engine::topic_bus::{TopicBusMessage, TopicBusTx};

/// Internal shared replay progress state.
struct ReplayProgress {
    state: ReplayState,
    session_id: Option<String>,
    progress: f64,
    elapsed_secs: f64,
    total_secs: f64,
    speed: f64,
    error: Option<String>,
    cancel: bool,
}

/// Manages MCAP replay lifecycle using a shared TopicBus.
pub struct ReplayManager {
    rt_handle: tokio::runtime::Handle,
    progress: Arc<Mutex<ReplayProgress>>,
    task_handle: Option<tokio::task::JoinHandle<()>>,
    bus_tx: TopicBusTx,
}

impl ReplayManager {
    pub fn new(bus_tx: TopicBusTx, rt_handle: tokio::runtime::Handle) -> Self {
        ReplayManager {
            rt_handle,
            progress: Arc::new(Mutex::new(ReplayProgress {
                state: ReplayState::Idle,
                session_id: None,
                progress: 0.0,
                elapsed_secs: 0.0,
                total_secs: 0.0,
                speed: 1.0,
                error: None,
                cancel: false,
            })),
            task_handle: None,
            bus_tx,
        }
    }

    /// Whether a replay is currently active (starting or running).
    pub fn is_active(&self) -> bool {
        let p = self.progress.lock().unwrap();
        matches!(p.state, ReplayState::Starting | ReplayState::Running)
    }

    /// Session ID of the current/last replay, if any.
    pub fn current_session_id(&self) -> Option<String> {
        let p = self.progress.lock().unwrap();
        if matches!(p.state, ReplayState::Starting | ReplayState::Running) {
            p.session_id.clone()
        } else {
            None
        }
    }

    /// Start replaying an MCAP file.
    /// Returns (total_secs, message_count) on success.
    pub fn start(
        &mut self,
        mcap_path: PathBuf,
        session_id: String,
        speed: f64,
    ) -> Result<(f64, u64), String> {
        if self.is_active() {
            return Err("another replay is already active, stop it first".into());
        }

        // Read MCAP file
        let data =
            std::fs::read(&mcap_path).map_err(|e| format!("read mcap file: {}", e))?;

        // Parse summary for metadata
        let (total_secs, message_count) = read_mcap_summary(&data);

        // Set starting state
        {
            let mut p = self.progress.lock().unwrap();
            p.state = ReplayState::Starting;
            p.session_id = Some(session_id.clone());
            p.progress = 0.0;
            p.elapsed_secs = 0.0;
            p.total_secs = total_secs;
            p.speed = speed;
            p.error = None;
            p.cancel = false;
        }

        // Send start control frame
        let ctrl = serde_json::json!({
            "event": "start",
            "session_id": session_id,
            "total_secs": total_secs,
            "message_count": message_count,
            "speed": speed,
        });
        let _ = self.bus_tx.try_send(TopicBusMessage::Data {
            topic: "/replay/status".into(),
            payload: serde_json::to_vec(&ctrl).unwrap(),
        });

        // Spawn async replay task
        let progress = self.progress.clone();
        let tx = self.bus_tx.clone();
        let handle = self.rt_handle.spawn(async move {
            replay_task(data, progress, speed, tx).await;
        });
        self.task_handle = Some(handle);

        eprintln!(
            "[replay] started session '{}' ({:.1}s, {} messages, speed={:.1}x)",
            session_id, total_secs, message_count, speed,
        );

        Ok((total_secs, message_count))
    }

    /// Stop the current replay. Idempotent.
    pub fn stop(&mut self) {
        let session_id;
        {
            let mut p = self.progress.lock().unwrap();
            if !matches!(p.state, ReplayState::Starting | ReplayState::Running) {
                // Already idle or completed — reset to idle
                p.state = ReplayState::Idle;
                p.session_id = None;
                return;
            }
            session_id = p.session_id.clone();
            p.state = ReplayState::Stopping;
            p.cancel = true;
        }

        if let Some(handle) = self.task_handle.take() {
            handle.abort();
        }

        // Send stop control frame (socket is still alive, so this will be delivered)
        let ctrl = serde_json::json!({
            "event": "stop",
            "session_id": session_id,
        });
        let _ = self.bus_tx.try_send(TopicBusMessage::Data {
            topic: "/replay/status".into(),
            payload: serde_json::to_vec(&ctrl).unwrap(),
        });

        {
            let mut p = self.progress.lock().unwrap();
            p.state = ReplayState::Idle;
            p.session_id = None;
            p.progress = 0.0;
            p.elapsed_secs = 0.0;
            p.total_secs = 0.0;
            p.error = None;
            p.cancel = false;
        }

        eprintln!("[replay] stopped");
    }

    /// Get current replay status.
    pub fn status(&self) -> ReplayStatusInfo {
        // Lazy cleanup: if the task has finished but state still shows active, update it
        let p = self.progress.lock().unwrap();
        ReplayStatusInfo {
            active: matches!(p.state, ReplayState::Starting | ReplayState::Running),
            state: p.state.clone(),
            session_id: p.session_id.clone(),
            progress: p.progress,
            elapsed_secs: p.elapsed_secs,
            total_secs: p.total_secs,
            speed: p.speed,
            error: p.error.clone(),
        }
    }
}

/// Read MCAP summary to extract total duration and message count.
/// Returns (0.0, 0) if summary cannot be read.
pub fn read_mcap_summary(data: &[u8]) -> (f64, u64) {
    let stream = match mcap::MessageStream::new(data) {
        Ok(s) => s,
        Err(_) => return (0.0, 0),
    };

    let mut first_time: Option<u64> = None;
    let mut last_time: u64 = 0;
    let mut count: u64 = 0;

    for msg_result in stream {
        if let Ok(msg) = msg_result {
            if first_time.is_none() {
                first_time = Some(msg.log_time);
            }
            last_time = msg.log_time;
            count += 1;
        }
    }

    let duration_secs = match first_time {
        Some(first) => (last_time.saturating_sub(first)) as f64 / 1e9,
        None => 0.0,
    };

    (duration_secs, count)
}

/// Async replay task: reads MCAP messages and publishes to ZMQ with timing.
async fn replay_task(
    data: Vec<u8>,
    progress: Arc<Mutex<ReplayProgress>>,
    speed: f64,
    tx: TopicBusTx,
) {
    // Collect all messages with their timestamps and topics
    let messages: Vec<(u64, String, Vec<u8>)> = match mcap::MessageStream::new(&data) {
        Ok(stream) => stream
            .filter_map(|r| r.ok())
            .map(|msg| (msg.log_time, msg.channel.topic.clone(), msg.data.to_vec()))
            .collect(),
        Err(e) => {
            eprintln!("[replay] failed to parse mcap: {}", e);
            let mut p = progress.lock().unwrap();
            p.state = ReplayState::Error;
            p.error = Some(format!("mcap parse failed: {}", e));
            return;
        }
    };

    if messages.is_empty() {
        let mut p = progress.lock().unwrap();
        p.state = ReplayState::Completed;
        p.progress = 1.0;
        return;
    }

    // Transition to running
    {
        let mut p = progress.lock().unwrap();
        p.state = ReplayState::Running;
    }

    // Slow-joiner mitigation: give subscribers time to connect
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let first_time = messages[0].0;
    let last_time = messages.last().map(|m| m.0).unwrap_or(first_time);
    let total_ns = last_time.saturating_sub(first_time);
    let started = Instant::now();

    for (i, (log_time, topic, payload)) in messages.iter().enumerate() {
        // Check cancellation
        {
            let p = progress.lock().unwrap();
            if p.cancel {
                return; // stop() handles state cleanup
            }
        }

        // Sleep to maintain timing
        if i > 0 {
            let delta_ns = log_time.saturating_sub(messages[i - 1].0);
            let sleep_ns = (delta_ns as f64 / speed) as u64;
            if sleep_ns > 1000 {
                tokio::time::sleep(std::time::Duration::from_nanos(sleep_ns)).await;
            }
        }

        // Send to ZMQ via TopicBus
        let _ = tx
            .send(TopicBusMessage::Data {
                topic: topic.clone(),
                payload: payload.clone(),
            })
            .await;

        // Update progress
        let elapsed = started.elapsed();
        let progress_ns = log_time.saturating_sub(first_time);
        {
            let mut p = progress.lock().unwrap();
            p.elapsed_secs = elapsed.as_secs_f64();
            p.progress = if total_ns > 0 {
                progress_ns as f64 / total_ns as f64
            } else {
                1.0
            };
        }
    }

    // Completed naturally
    let mut p = progress.lock().unwrap();
    p.state = ReplayState::Completed;
    p.progress = 1.0;
    eprintln!("[replay] completed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::recorder::mcap_writer::McapSession;
    use crate::engine::topic_bus::TopicBus;

    /// Helper: create a test MCAP file with some mask messages.
    fn create_test_mcap(dir: &std::path::Path, session_id: &str) -> PathBuf {
        let dir_str = dir.to_str().unwrap();
        let mut session =
            McapSession::new(session_id, dir_str).expect("McapSession::new failed");
        session.register_schemas().expect("register_schemas failed");

        for i in 0..10 {
            let mask_json = serde_json::json!({
                "type": "hardware_mask",
                "mask": 0x01,
                "sequence": i,
            });
            let bytes = serde_json::to_vec(&mask_json).unwrap();
            let ts = 1_000_000_000u64 + i * 100_000_000; // 100ms apart
            session.write_mask(&bytes, ts).expect("write_mask failed");
        }

        session.finish().expect("finish failed")
    }

    #[test]
    fn test_read_mcap_summary() {
        let dir = std::env::temp_dir().join("rapid_test_replay_summary");
        let _ = std::fs::remove_dir_all(&dir);

        let path = create_test_mcap(&dir, "test_summary");
        let data = std::fs::read(&path).unwrap();

        let (duration, count) = read_mcap_summary(&data);
        assert!(count > 0, "expected messages, got {}", count);
        assert!(duration > 0.0, "expected positive duration, got {}", duration);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_replay_manager_status_idle() {
        let bus = TopicBus::new("tcp://127.0.0.1:15560".into());
        let mgr = ReplayManager::new(bus.sender(), bus.runtime_handle());
        let status = mgr.status();
        assert!(!status.active);
        assert_eq!(status.state, ReplayState::Idle);
        assert!(status.session_id.is_none());
    }

    #[test]
    fn test_replay_start_stop() {
        let dir = std::env::temp_dir().join("rapid_test_replay_start");
        let _ = std::fs::remove_dir_all(&dir);

        let path = create_test_mcap(&dir, "test_replay");

        let bus = TopicBus::new("tcp://127.0.0.1:15561".into());
        let mut mgr = ReplayManager::new(bus.sender(), bus.runtime_handle());
        let result = mgr.start(path, "test_replay".into(), 10.0);
        assert!(result.is_ok(), "start failed: {:?}", result);

        let (total_secs, msg_count) = result.unwrap();
        assert!(total_secs > 0.0);
        assert!(msg_count > 0);

        // Status should show active
        std::thread::sleep(std::time::Duration::from_millis(200));
        let status = mgr.status();
        assert!(
            matches!(status.state, ReplayState::Starting | ReplayState::Running | ReplayState::Completed),
            "unexpected state: {:?}",
            status.state
        );

        // Stop
        mgr.stop();
        let status = mgr.status();
        assert_eq!(status.state, ReplayState::Idle);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_replay_nonexistent_file() {
        let bus = TopicBus::new("tcp://127.0.0.1:15562".into());
        let mut mgr = ReplayManager::new(bus.sender(), bus.runtime_handle());
        let result = mgr.start("/nonexistent/path.mcap".into(), "bad".into(), 1.0);
        assert!(result.is_err());
    }
}
