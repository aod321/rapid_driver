//! MCAP session management — writes sensor messages to an MCAP file.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::PathBuf;
use std::time::Instant;

use mcap::Compression;
use mcap::records::MessageHeader;
use mcap::write::{WriteOptions, Writer};

use super::messages::SensorMessage;
use super::schemas;

/// An active MCAP recording session.
pub struct McapSession {
    writer: Writer<BufWriter<File>>,
    schema_ids: HashMap<String, u16>,  // schema_name -> schema_id
    channel_ids: HashMap<String, u16>, // topic -> channel_id
    pub session_id: String,
    pub output_path: PathBuf,
    pub message_count: u64,
    pub started_at: Instant,
    sequences: HashMap<u16, u32>, // channel_id -> sequence counter
    unknown_types_warned: HashSet<String>,
}

impl McapSession {
    /// Create a new MCAP session, creating the output directory and file.
    pub fn new(session_id: &str, output_dir: &str) -> Result<Self, String> {
        let output_dir = PathBuf::from(output_dir);
        fs::create_dir_all(&output_dir).map_err(|e| format!("create output dir: {}", e))?;

        let output_path = output_dir.join(format!("{}.mcap", session_id));
        let file = File::create(&output_path).map_err(|e| format!("create mcap file: {}", e))?;
        let buf = BufWriter::new(file);

        let writer = WriteOptions::new()
            .compression(Some(Compression::Zstd))
            .profile("zumi")
            .create(buf)
            .map_err(|e| format!("create mcap writer: {}", e))?;

        Ok(McapSession {
            writer,
            schema_ids: HashMap::new(),
            channel_ids: HashMap::new(),
            session_id: session_id.to_string(),
            output_path,
            message_count: 0,
            started_at: Instant::now(),
            sequences: HashMap::new(),
            unknown_types_warned: HashSet::new(),
        })
    }

    /// Register all known schemas with the MCAP writer.
    pub fn register_schemas(&mut self) -> Result<(), String> {
        for (name, json_schema) in schemas::all_schemas() {
            let schema_id = self
                .writer
                .add_schema(name, "jsonschema", json_schema.as_bytes())
                .map_err(|e| format!("add schema '{}': {}", name, e))?;
            self.schema_ids.insert(name.to_string(), schema_id);
        }
        Ok(())
    }

    /// Create a channel for the given topic and schema name.
    /// Returns the channel_id.
    fn get_or_create_channel(&mut self, topic: &str, schema_name: &str) -> Result<u16, String> {
        if let Some(&id) = self.channel_ids.get(topic) {
            return Ok(id);
        }

        let schema_id = self
            .schema_ids
            .get(schema_name)
            .copied()
            .ok_or_else(|| format!("unknown schema: {}", schema_name))?;

        let metadata = BTreeMap::new();
        let channel_id = self
            .writer
            .add_channel(schema_id, topic, "json", &metadata)
            .map_err(|e| format!("add channel '{}': {}", topic, e))?;

        self.channel_ids.insert(topic.to_string(), channel_id);
        Ok(channel_id)
    }

    /// Write a sensor message to the MCAP file.
    /// Determines the topic from source_name + msg_type, creates channels lazily.
    pub fn write_message(&mut self, msg: &SensorMessage) -> Result<(), String> {
        let Some((schema_name, suffix)) = schemas::msg_type_to_schema_and_suffix(&msg.msg_type)
        else {
            if self.unknown_types_warned.insert(msg.msg_type.clone()) {
                eprintln!(
                    "[recorder] skipping unknown msg type '{}' from '{}'",
                    msg.msg_type, msg.source_name
                );
            }
            return Ok(());
        };

        let topic = format!("/{}{}", msg.source_name, suffix);
        let channel_id = self.get_or_create_channel(&topic, schema_name)?;

        let seq = self.sequences.entry(channel_id).or_insert(0);
        *seq += 1;

        let header = MessageHeader {
            channel_id,
            sequence: *seq,
            log_time: msg.timestamp_ns,
            publish_time: msg.timestamp_ns,
        };

        self.writer
            .write_to_known_channel(&header, &msg.json_data)
            .map_err(|e| format!("write message: {}", e))?;

        self.message_count += 1;
        Ok(())
    }

    /// Write a hardware mask snapshot message.
    pub fn write_mask(&mut self, mask_json: &[u8], timestamp_ns: u64) -> Result<(), String> {
        let topic = "/hardware_mask";
        let schema_name = "zumi.HardwareMask";
        let channel_id = self.get_or_create_channel(topic, schema_name)?;

        let seq = self.sequences.entry(channel_id).or_insert(0);
        *seq += 1;

        let header = MessageHeader {
            channel_id,
            sequence: *seq,
            log_time: timestamp_ns,
            publish_time: timestamp_ns,
        };

        self.writer
            .write_to_known_channel(&header, mask_json)
            .map_err(|e| format!("write mask: {}", e))?;

        self.message_count += 1;
        Ok(())
    }

    /// Finish writing the MCAP file and return the output path.
    pub fn finish(mut self) -> Result<PathBuf, String> {
        let path = self.output_path.clone();
        self.writer
            .finish()
            .map_err(|e| format!("finish mcap: {}", e))?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcap_create_and_finish() {
        let dir = std::env::temp_dir().join("rapid_test_mcap");
        let _ = fs::remove_dir_all(&dir);

        let mut session = McapSession::new("test_session", dir.to_str().unwrap())
            .expect("McapSession::new failed");
        session.register_schemas().expect("register_schemas failed");

        // Write a mask message
        let mask_json = serde_json::json!({"type":"hardware_mask","mask":0,"sequence":0});
        let bytes = serde_json::to_vec(&mask_json).unwrap();
        session.write_mask(&bytes, 1000).expect("write_mask failed");

        let path = session.finish().expect("finish failed");
        assert!(path.exists(), "mcap file not found: {}", path.display());
        let meta = fs::metadata(&path).unwrap();
        assert!(meta.len() > 0, "mcap file is empty");
        println!("OK: {} ({} bytes)", path.display(), meta.len());

        let _ = fs::remove_dir_all(&dir);
    }
}
