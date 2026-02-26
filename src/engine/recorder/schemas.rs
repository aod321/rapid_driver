//! JSON Schema constants for MCAP recording channels.

/// Returns (schema_name, json_schema_string) for motor state messages.
pub fn motor_schema() -> (&'static str, &'static str) {
    ("zumi.MotorState", r#"{
  "type": "object",
  "properties": {
    "type": {"type": "string"},
    "ts": {"type": "number"},
    "pos": {"type": "array", "items": {"type": "number"}},
    "vel": {"type": "array", "items": {"type": "number"}},
    "tau": {"type": "array", "items": {"type": "number"}},
    "mode": {"type": "array", "items": {"type": "integer"}},
    "enabled": {"type": "array", "items": {"type": "boolean"}},
    "fault": {"type": "array", "items": {"type": "integer"}}
  }
}"#)
}

/// Returns (schema_name, json_schema_string) for video packet messages.
pub fn video_schema() -> (&'static str, &'static str) {
    ("zumi.VideoPacket", r#"{
  "type": "object",
  "properties": {
    "type": {"type": "string"},
    "ts": {"type": "number"},
    "data": {"type": "string", "contentEncoding": "base64"},
    "codec": {"type": "string"},
    "is_keyframe": {"type": "boolean"},
    "width": {"type": "integer"},
    "height": {"type": "integer"},
    "frame_index": {"type": "integer"},
    "pts": {"type": "number"}
  }
}"#)
}

/// Returns (schema_name, json_schema_string) for image packet messages.
pub fn image_schema() -> (&'static str, &'static str) {
    ("foxglove.CompressedImage", r#"{
  "type": "object",
  "properties": {
    "timestamp": {
      "type": "object",
      "properties": {
        "sec": {"type": "integer"},
        "nsec": {"type": "integer"}
      },
      "required": ["sec", "nsec"]
    },
    "frame_id": {"type": "string"},
    "format": {"type": "string"},
    "data": {"type": "string", "contentEncoding": "base64"}
  },
  "required": ["timestamp", "frame_id", "format", "data"]
}"#)
}

/// Returns (schema_name, json_schema_string) for iPhone pose messages.
pub fn iphone_pose_schema() -> (&'static str, &'static str) {
    ("foxglove.PoseInFrame", r#"{
  "type": "object",
  "properties": {
    "timestamp": {
      "type": "object",
      "properties": {
        "sec": {"type": "integer"},
        "nsec": {"type": "integer"}
      },
      "required": ["sec", "nsec"]
    },
    "frame_id": {"type": "string"},
    "pose": {
      "type": "object",
      "properties": {
        "position": {
          "type": "object",
          "properties": {
            "x": {"type": "number"},
            "y": {"type": "number"},
            "z": {"type": "number"}
          },
          "required": ["x", "y", "z"]
        },
        "orientation": {
          "type": "object",
          "properties": {
            "x": {"type": "number"},
            "y": {"type": "number"},
            "z": {"type": "number"},
            "w": {"type": "number"}
          },
          "required": ["x", "y", "z", "w"]
        }
      },
      "required": ["position", "orientation"]
    }
  },
  "required": ["timestamp", "frame_id", "pose"]
}"#)
}

/// Returns (schema_name, json_schema_string) for GoPro metadata messages.
pub fn gopro_meta_schema() -> (&'static str, &'static str) {
    ("zumi.GoProMetadata", r#"{
  "type": "object",
  "properties": {
    "type": {"type": "string"},
    "ts": {"type": "number"},
    "run_id": {"type": "string"},
    "episode": {"type": "integer"},
    "session_id": {"type": "string"}
  }
}"#)
}

/// Returns (schema_name, json_schema_string) for hardware mask messages.
pub fn hardware_mask_schema() -> (&'static str, &'static str) {
    ("zumi.HardwareMask", r#"{
  "type": "object",
  "properties": {
    "type": {"type": "string"},
    "ts": {"type": "number"},
    "mask": {"type": "integer"},
    "sequence": {"type": "integer"},
    "device_count": {"type": "integer"},
    "devices": {"type": "object", "additionalProperties": {"type": "boolean"}}
  }
}"#)
}

/// Map a message type string to its (schema_name, topic_suffix).
/// Returns None for unknown message types.
pub fn msg_type_to_schema_and_suffix(msg_type: &str) -> Option<(&'static str, &'static str)> {
    match msg_type {
        "motor" => Some(("zumi.MotorState", "/state")),
        "video" => Some(("zumi.VideoPacket", "/video")),
        "image" => Some(("foxglove.CompressedImage", "/image")),
        "iphone_pose" => Some(("foxglove.PoseInFrame", "/pose")),
        "recording_start" | "recording_stop" => Some(("zumi.GoProMetadata", "/metadata")),
        _ => None,
    }
}

/// Get all schemas as (name, json_bytes) pairs for MCAP registration.
pub fn all_schemas() -> Vec<(&'static str, &'static str)> {
    vec![
        motor_schema(),
        video_schema(),
        image_schema(),
        iphone_pose_schema(),
        gopro_meta_schema(),
        hardware_mask_schema(),
    ]
}

#[cfg(test)]
mod tests {
    use super::{all_schemas, msg_type_to_schema_and_suffix};

    #[test]
    fn maps_image_type() {
        assert_eq!(
            msg_type_to_schema_and_suffix("image"),
            Some(("foxglove.CompressedImage", "/image"))
        );
    }

    #[test]
    fn maps_iphone_pose_type() {
        assert_eq!(
            msg_type_to_schema_and_suffix("iphone_pose"),
            Some(("foxglove.PoseInFrame", "/pose"))
        );
    }

    #[test]
    fn registers_compressed_image_schema() {
        let has_image = all_schemas()
            .iter()
            .any(|(name, _)| *name == "foxglove.CompressedImage");
        assert!(has_image, "missing foxglove.CompressedImage schema registration");
    }

    #[test]
    fn registers_iphone_pose_schema() {
        let has_pose = all_schemas()
            .iter()
            .any(|(name, _)| *name == "foxglove.PoseInFrame");
        assert!(has_pose, "missing foxglove.PoseInFrame schema registration");
    }
}
