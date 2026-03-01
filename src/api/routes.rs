//! HTTP handler functions.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::engine::command::{EngineCommand, EngineHandle, EngineResponse};
use crate::registry::DeviceEntry;

pub fn router() -> Router<EngineHandle> {
    Router::new()
        .route("/status", get(get_status))
        .route("/devices", get(get_devices))
        .route("/devices/{name}", get(get_device))
        .route("/devices/{name}/restart", post(restart_device))
        .route("/devices/{name}/stop", post(stop_device))
        .route("/devices/{name}/start", post(start_device))
        .route("/registry", get(get_registry))
        .route("/registry", post(add_device))
        .route("/registry/{name}", delete(remove_device))
        .route("/mask", get(get_mask))
        .route("/ready", get(get_ready))
        .route("/recording/start", post(start_recording))
        .route("/recording/stop", post(stop_recording))
        .route("/recording/status", get(get_recording_status))
        .route("/layout/move", post(move_layout))
        .route("/layout/save", post(save_layout))
        .route("/layout/export", post(export_layout))
        .route("/layout/import", post(import_layout))
        .route("/shutdown", post(shutdown))
        // Recordings management
        .route("/recordings", get(list_recordings))
        .route("/recordings/{session_id}", delete(delete_recording))
        .route("/recordings/delete_batch", post(delete_recordings_batch))
        // Replay
        .route("/recordings/{session_id}/replay", post(start_replay))
        .route("/replay/stop", post(stop_replay))
        .route("/replay/status", get(get_replay_status))
}

/// Helper: run a blocking engine request on the tokio blocking pool.
async fn engine_request(
    handle: EngineHandle,
    cmd: EngineCommand,
) -> Result<EngineResponse, StatusCode> {
    tokio::task::spawn_blocking(move || handle.request(cmd))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
}

// ── Handlers ────────────────────────────────────────────────────

async fn get_status(State(handle): State<EngineHandle>) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::GetStatus).await {
        Ok(EngineResponse::Status(status)) => Json(serde_json::json!(status)).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn get_devices(State(handle): State<EngineHandle>) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::GetDevices).await {
        Ok(EngineResponse::Devices(devices)) => Json(serde_json::json!(devices)).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn get_device(
    State(handle): State<EngineHandle>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::GetDevice { name }).await {
        Ok(EngineResponse::Device(Some(device))) => Json(serde_json::json!(device)).into_response(),
        Ok(EngineResponse::Device(None)) => StatusCode::NOT_FOUND.into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn restart_device(
    State(handle): State<EngineHandle>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::RestartDevice { name }).await {
        Ok(EngineResponse::Ok) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(EngineResponse::Error(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn stop_device(
    State(handle): State<EngineHandle>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::StopDevice { name }).await {
        Ok(EngineResponse::Ok) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(EngineResponse::Error(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn start_device(
    State(handle): State<EngineHandle>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::StartDevice { name }).await {
        Ok(EngineResponse::Ok) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(EngineResponse::Error(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn get_registry(State(handle): State<EngineHandle>) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::GetRegistry).await {
        Ok(EngineResponse::Registry(reg)) => Json(serde_json::json!(reg)).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
struct AddDeviceRequest {
    name: String,
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    vid: Option<String>,
    #[serde(default)]
    pid: Option<String>,
    #[serde(default)]
    serial: Option<String>,
    #[serde(default)]
    service_type: Option<String>,
    #[serde(default)]
    on_attach: Option<String>,
    #[serde(default)]
    on_detach: Option<String>,
    #[serde(default)]
    on_record_start: Option<String>,
    #[serde(default)]
    on_record_stop: Option<String>,
    #[serde(default)]
    sensor_type: Option<String>,
}

async fn add_device(
    State(handle): State<EngineHandle>,
    Json(req): Json<AddDeviceRequest>,
) -> impl IntoResponse {
    let entry = DeviceEntry {
        name: req.name,
        backend: req.backend.unwrap_or_else(|| "usb".into()),
        vid: req.vid.unwrap_or_default(),
        pid: req.pid.unwrap_or_default(),
        serial: req.serial.unwrap_or_default(),
        service_type: req.service_type.unwrap_or_default(),
        on_attach: req.on_attach.unwrap_or_default(),
        on_detach: req.on_detach.unwrap_or_default(),
        on_record_start: req.on_record_start.unwrap_or_default(),
        on_record_stop: req.on_record_stop.unwrap_or_default(),
        sensor_type: req.sensor_type.unwrap_or_default(),
    };
    match engine_request(handle, EngineCommand::AddDevice { entry }).await {
        Ok(EngineResponse::Ok) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(EngineResponse::Error(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn remove_device(
    State(handle): State<EngineHandle>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::RemoveDevice { name }).await {
        Ok(EngineResponse::Ok) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(EngineResponse::Error(e)) => {
            (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": e}))).into_response()
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn get_ready(State(handle): State<EngineHandle>) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::GetStatus).await {
        Ok(EngineResponse::Status(status)) => {
            let total = status.devices.len();
            let online = status
                .devices
                .iter()
                .filter(|d| d.process_running && d.heartbeat_ok.unwrap_or(true))
                .count();
            let mut reasons: HashMap<String, String> = HashMap::new();
            for device in &status.devices {
                let available = device.process_running && device.heartbeat_ok.unwrap_or(true);
                if !available {
                    reasons.insert(
                        device.name.clone(),
                        device
                            .state_reason
                            .clone()
                            .unwrap_or_else(|| "unavailable".to_string()),
                    );
                }
            }
            let ready = total > 0 && online == total;
            Json(serde_json::json!({
                "ready": ready,
                "online": online,
                "total": total,
                "reasons": reasons,
            }))
            .into_response()
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn get_mask(State(handle): State<EngineHandle>) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::GetMask).await {
        Ok(EngineResponse::Mask { value, hex, layout }) => Json(serde_json::json!({
            "value": value,
            "hex": hex,
            "layout": layout,
        }))
        .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
struct MoveLayoutRequest {
    from: usize,
    to: usize,
}

async fn move_layout(
    State(handle): State<EngineHandle>,
    Json(req): Json<MoveLayoutRequest>,
) -> impl IntoResponse {
    match engine_request(
        handle,
        EngineCommand::MoveDevice {
            from: req.from,
            to: req.to,
        },
    )
    .await
    {
        Ok(EngineResponse::Ok) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(EngineResponse::Error(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn save_layout(State(handle): State<EngineHandle>) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::SaveLayout).await {
        Ok(EngineResponse::Ok) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(EngineResponse::Error(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
struct LayoutPathRequest {
    path: String,
}

async fn export_layout(
    State(handle): State<EngineHandle>,
    Json(req): Json<LayoutPathRequest>,
) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::ExportLayout { path: req.path }).await {
        Ok(EngineResponse::Ok) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(EngineResponse::Error(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn import_layout(
    State(handle): State<EngineHandle>,
    Json(req): Json<LayoutPathRequest>,
) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::ImportLayout { path: req.path }).await {
        Ok(EngineResponse::Ok) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(EngineResponse::Error(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
struct StartRecordingRequest {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    devices: Option<Vec<String>>,
    #[serde(default)]
    output_dir: Option<String>,
}

async fn start_recording(
    State(handle): State<EngineHandle>,
    Json(req): Json<StartRecordingRequest>,
) -> impl IntoResponse {
    match engine_request(
        handle,
        EngineCommand::StartRecording {
            session_id: req.session_id,
            devices: req.devices,
            output_dir: req.output_dir,
        },
    )
    .await
    {
        Ok(EngineResponse::Ok) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(EngineResponse::Error(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
struct StopRecordingRequest {
    #[serde(default)]
    session_id: Option<String>,
}

async fn stop_recording(
    State(handle): State<EngineHandle>,
    Json(req): Json<StopRecordingRequest>,
) -> impl IntoResponse {
    match engine_request(
        handle,
        EngineCommand::StopRecording {
            session_id: req.session_id,
        },
    )
    .await
    {
        Ok(EngineResponse::Ok) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(EngineResponse::Error(e)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn get_recording_status(State(handle): State<EngineHandle>) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::GetRecordingStatus).await {
        Ok(EngineResponse::RecordingStatus(info)) => {
            Json(serde_json::json!({"recording": info})).into_response()
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn shutdown(State(handle): State<EngineHandle>) -> impl IntoResponse {
    let _ = engine_request(handle, EngineCommand::Shutdown).await;
    Json(serde_json::json!({"ok": true, "message": "shutting down"}))
}

// ── Recordings management ──────────────────────────────────────────

/// Helper: map ErrorWithStatus to proper HTTP response.
fn error_with_status(status: u16, message: String) -> axum::response::Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (code, Json(serde_json::json!({"error": message}))).into_response()
}

#[derive(Deserialize)]
struct ListRecordingsQuery {
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    order: Option<String>,
}

async fn list_recordings(
    State(handle): State<EngineHandle>,
    Query(query): Query<ListRecordingsQuery>,
) -> impl IntoResponse {
    match engine_request(
        handle,
        EngineCommand::ListRecordings {
            sort: query.sort,
            order: query.order,
        },
    )
    .await
    {
        Ok(EngineResponse::RecordingsList {
            recordings,
            total_size_bytes,
            disk_free_bytes,
        }) => Json(serde_json::json!({
            "recordings": recordings,
            "total_size_bytes": total_size_bytes,
            "disk_free_bytes": disk_free_bytes,
        }))
        .into_response(),
        Ok(EngineResponse::ErrorWithStatus { status, message }) => {
            error_with_status(status, message)
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn delete_recording(
    State(handle): State<EngineHandle>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    match engine_request(
        handle,
        EngineCommand::DeleteRecording { session_id },
    )
    .await
    {
        Ok(EngineResponse::Deleted(sid)) => {
            Json(serde_json::json!({"deleted": sid})).into_response()
        }
        Ok(EngineResponse::ErrorWithStatus { status, message }) => {
            error_with_status(status, message)
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
struct BatchDeleteRequest {
    #[serde(default)]
    session_ids: Option<Vec<String>>,
}

async fn delete_recordings_batch(
    State(handle): State<EngineHandle>,
    Json(req): Json<BatchDeleteRequest>,
) -> impl IntoResponse {
    let session_ids = match req.session_ids {
        Some(ids) if !ids.is_empty() => ids,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "session_ids is required and must not be empty"
                })),
            )
                .into_response();
        }
    };

    match engine_request(
        handle,
        EngineCommand::DeleteRecordingsBatch { session_ids },
    )
    .await
    {
        Ok(EngineResponse::BatchDeleteResult { deleted, failed }) => {
            Json(serde_json::json!({
                "deleted": deleted,
                "failed": failed,
            }))
            .into_response()
        }
        Ok(EngineResponse::ErrorWithStatus { status, message }) => {
            error_with_status(status, message)
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

// ── Replay ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct StartReplayRequest {
    #[serde(default)]
    speed: Option<f64>,
}

async fn start_replay(
    State(handle): State<EngineHandle>,
    Path(session_id): Path<String>,
    Json(req): Json<StartReplayRequest>,
) -> impl IntoResponse {
    match engine_request(
        handle,
        EngineCommand::StartReplay {
            session_id,
            speed: req.speed,
        },
    )
    .await
    {
        Ok(EngineResponse::ReplayStarted {
            session_id,
            total_secs,
            message_count,
        }) => Json(serde_json::json!({
            "session_id": session_id,
            "total_secs": total_secs,
            "message_count": message_count,
        }))
        .into_response(),
        Ok(EngineResponse::ErrorWithStatus { status, message }) => {
            error_with_status(status, message)
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn stop_replay(State(handle): State<EngineHandle>) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::StopReplay).await {
        Ok(EngineResponse::Ok) => Json(serde_json::json!({"stopped": true})).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn get_replay_status(State(handle): State<EngineHandle>) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::GetReplayStatus).await {
        Ok(EngineResponse::ReplayStatus(info)) => Json(serde_json::json!(info)).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
