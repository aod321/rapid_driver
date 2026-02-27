//! Async ZMQ subscriber task for receiving sensor data.

use std::sync::mpsc::Sender;

use zeromq::{Socket, SocketRecv, SubSocket};

use super::messages::{SensorMessage, msgpack_to_sensor_message};

/// Spawn an async ZMQ SUB task that connects to a sensor's PUB socket,
/// decodes msgpack messages, and sends them through the channel.
///
/// This task runs until cancelled via `JoinHandle::abort()`.
pub async fn zmq_subscribe_task(address: String, source_name: String, tx: Sender<SensorMessage>) {
    let mut socket = SubSocket::new();
    socket.subscribe("").await.unwrap_or_else(|e| {
        eprintln!("[recorder] {}: subscribe error: {}", source_name, e);
    });

    let endpoint = format!("tcp://{}", address);
    if let Err(e) = socket.connect(&endpoint).await {
        eprintln!(
            "[recorder] {}: connect to {} failed: {}",
            source_name, endpoint, e
        );
        return;
    }
    eprintln!("[recorder] {}: connected to {}", source_name, endpoint);

    loop {
        match socket.recv().await {
            Ok(msg) => {
                let raw: Vec<u8> = msg
                    .into_vec()
                    .into_iter()
                    .flat_map(|frame| frame.to_vec())
                    .collect();

                match msgpack_to_sensor_message(&source_name, &raw) {
                    Ok(sensor_msg) => {
                        if tx.send(sensor_msg).is_err() {
                            break; // channel closed
                        }
                    }
                    Err(e) => {
                        eprintln!("[recorder] {}: decode error: {}", source_name, e);
                    }
                }
            }
            Err(e) => {
                eprintln!("[recorder] {}: recv error: {}", source_name, e);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}
