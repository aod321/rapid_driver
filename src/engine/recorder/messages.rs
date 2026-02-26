//! Sensor message types and msgpack → JSON conversion.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rmpv::Value;
use std::io::Cursor;

/// A decoded sensor message ready for MCAP writing.
pub struct SensorMessage {
    pub source_name: String,
    pub msg_type: String,
    pub timestamp_ns: u64,
    pub json_data: Vec<u8>,
}

/// Convert raw msgpack bytes into a SensorMessage.
///
/// Extracts `type` and `ts` fields, converts binary values to base64,
/// then serializes the whole map as JSON.
pub fn msgpack_to_sensor_message(source_name: &str, raw: &[u8]) -> Result<SensorMessage, String> {
    let mut cursor = Cursor::new(raw);
    let value = rmpv::decode::read_value(&mut cursor)
        .map_err(|e| format!("msgpack decode: {}", e))?;

    let Value::Map(ref entries) = value else {
        return Err("expected msgpack Map at top level".into());
    };

    // Extract "type" field
    let msg_type = entries
        .iter()
        .find_map(|(k, v)| {
            if k.as_str()? == "type" {
                v.as_str().map(|s| s.to_string())
            } else {
                None
            }
        })
        .unwrap_or_default();

    // Extract "ts" field as nanoseconds
    let timestamp_ns = entries
        .iter()
        .find_map(|(k, v)| {
            if k.as_str()? == "ts" {
                v.as_f64().map(|f| (f * 1e9) as u64)
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        });

    // Convert to JSON with binary → base64
    let json_value = rmpv_to_json(&value);
    let json_data = serde_json::to_vec(&json_value)
        .map_err(|e| format!("json serialize: {}", e))?;

    Ok(SensorMessage {
        source_name: source_name.to_string(),
        msg_type,
        timestamp_ns,
        json_data,
    })
}

/// Recursively convert rmpv::Value to serde_json::Value.
/// Binary values are base64-encoded to strings.
fn rmpv_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Nil => serde_json::Value::Null,
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::Integer(i) => {
            if let Some(n) = i.as_i64() {
                serde_json::Value::Number(n.into())
            } else if let Some(n) = i.as_u64() {
                serde_json::Value::Number(n.into())
            } else {
                serde_json::Value::Null
            }
        }
        Value::F32(f) => {
            serde_json::Number::from_f64(*f as f64)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null)
        }
        Value::F64(f) => {
            serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null)
        }
        Value::String(s) => {
            serde_json::Value::String(s.as_str().unwrap_or("").to_string())
        }
        Value::Binary(b) => {
            serde_json::Value::String(BASE64.encode(b))
        }
        Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(rmpv_to_json).collect())
        }
        Value::Map(entries) => {
            let mut map = serde_json::Map::new();
            for (k, v) in entries {
                let key = match k {
                    Value::String(s) => s.as_str().unwrap_or("").to_string(),
                    Value::Integer(i) => i.to_string(),
                    _ => format!("{}", k),
                };
                map.insert(key, rmpv_to_json(v));
            }
            serde_json::Value::Object(map)
        }
        Value::Ext(_, data) => {
            serde_json::Value::String(BASE64.encode(data))
        }
    }
}
