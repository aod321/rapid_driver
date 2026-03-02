//! TopicBus — shared ZMQ PUB socket with channel-based publish API.
//!
//! Owns a tokio runtime, a PUB socket, and background tasks for heartbeat
//! and topic catalog. Publishers (e.g. ReplayManager) send messages via
//! an mpsc channel obtained from `sender()`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Messages sent through the TopicBus channel.
pub enum TopicBusMessage {
    Data { topic: String, payload: Vec<u8> },
    Shutdown,
}

/// Sender half — clone this for each publisher.
pub type TopicBusTx = tokio::sync::mpsc::Sender<TopicBusMessage>;

/// Per-topic statistics.
struct TopicStats {
    count: u64,
    /// Rolling window of timestamps for Hz calculation.
    timestamps: Vec<Instant>,
}

impl TopicStats {
    fn new() -> Self {
        TopicStats {
            count: 0,
            timestamps: Vec::new(),
        }
    }

    fn record(&mut self, now: Instant) {
        self.count += 1;
        self.timestamps.push(now);
        // Keep only last 5 seconds
        let cutoff = now - std::time::Duration::from_secs(5);
        self.timestamps.retain(|t| *t >= cutoff);
    }

    fn hz(&self, now: Instant) -> f64 {
        let cutoff = now - std::time::Duration::from_secs(5);
        let recent: Vec<_> = self.timestamps.iter().filter(|t| **t >= cutoff).collect();
        if recent.len() < 2 {
            return 0.0;
        }
        let window = now.duration_since(*recent[0]).as_secs_f64();
        if window < 0.001 {
            return 0.0;
        }
        recent.len() as f64 / window
    }
}

type StatsMap = Arc<Mutex<HashMap<String, TopicStats>>>;

/// Shared ZMQ PUB socket with heartbeat and catalog tasks.
pub struct TopicBus {
    rt: tokio::runtime::Runtime,
    tx: TopicBusTx,
    #[allow(dead_code)]
    stats: StatsMap,
}

impl TopicBus {
    /// Create a new TopicBus that binds a PUB socket to `endpoint`.
    pub fn new(endpoint: String) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("topic-bus")
            .build()
            .expect("failed to build topic-bus tokio runtime");

        let (tx, rx) = tokio::sync::mpsc::channel::<TopicBusMessage>(512);
        let stats: StatsMap = Arc::new(Mutex::new(HashMap::new()));

        // Spawn the PUB publisher task
        let ep = endpoint.clone();
        let stats_clone = stats.clone();
        rt.spawn(pub_publisher_task(ep, rx, stats_clone));

        // Spawn heartbeat task (publishes to /heartbeat every 1s)
        let hb_tx = tx.clone();
        rt.spawn(heartbeat_task(hb_tx));

        // Spawn catalog task (publishes to /topics/list every 1s)
        let cat_tx = tx.clone();
        let cat_stats = stats.clone();
        rt.spawn(catalog_task(cat_stats, cat_tx));

        TopicBus { rt, tx, stats }
    }

    /// Get a clone of the sender for publishing messages.
    pub fn sender(&self) -> TopicBusTx {
        self.tx.clone()
    }

    /// Get a handle to the runtime for spawning tasks.
    pub fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.rt.handle().clone()
    }
}

impl Drop for TopicBus {
    fn drop(&mut self) {
        let _ = self.tx.try_send(TopicBusMessage::Shutdown);
    }
}

/// PUB socket task — binds once, forwards channel messages as ZMQ frames.
async fn pub_publisher_task(
    endpoint: String,
    mut rx: tokio::sync::mpsc::Receiver<TopicBusMessage>,
    stats: StatsMap,
) {
    use bytes::Bytes;
    use zeromq::{PubSocket, Socket, SocketSend};

    let mut socket = PubSocket::new();
    if let Err(e) = socket.bind(&endpoint).await {
        eprintln!("[topic-bus] failed to bind to {}: {}", endpoint, e);
        return;
    }
    eprintln!("[topic-bus] PUB bound to {}", endpoint);

    while let Some(msg) = rx.recv().await {
        match msg {
            TopicBusMessage::Data { topic, payload } => {
                // Update stats
                {
                    let mut s = stats.lock().unwrap();
                    let entry = s.entry(topic.clone()).or_insert_with(TopicStats::new);
                    entry.record(Instant::now());
                }

                let mut zmq_msg = zeromq::ZmqMessage::from(Bytes::from(topic.into_bytes()));
                zmq_msg.push_back(Bytes::from(payload));
                let _ = socket.send(zmq_msg).await;
            }
            TopicBusMessage::Shutdown => {
                eprintln!("[topic-bus] shutting down");
                break;
            }
        }
    }
}

/// Heartbeat task — publishes JSON to `/heartbeat` every 1s.
async fn heartbeat_task(tx: TopicBusTx) {
    let started = Instant::now();
    let pid = std::process::id();

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let payload = serde_json::json!({
            "timestamp": now.as_secs_f64(),
            "uptime_secs": started.elapsed().as_secs_f64(),
            "pid": pid,
        });
        let bytes = serde_json::to_vec(&payload).unwrap();

        if tx
            .send(TopicBusMessage::Data {
                topic: "/heartbeat".into(),
                payload: bytes,
            })
            .await
            .is_err()
        {
            break; // channel closed
        }
    }
}

/// Catalog task — publishes topic list JSON to `/topics/list` every 1s.
async fn catalog_task(stats: StatsMap, tx: TopicBusTx) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let snapshot: Vec<serde_json::Value> = {
            let s = stats.lock().unwrap();
            let now = Instant::now();
            s.iter()
                .map(|(name, st)| {
                    serde_json::json!({
                        "name": name,
                        "count": st.count,
                        "hz": (st.hz(now) * 10.0).round() / 10.0,
                    })
                })
                .collect()
        };

        let bytes = serde_json::to_vec(&snapshot).unwrap();
        if tx
            .send(TopicBusMessage::Data {
                topic: "/topics/list".into(),
                payload: bytes,
            })
            .await
            .is_err()
        {
            break;
        }
    }
}
