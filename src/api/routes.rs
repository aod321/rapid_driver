//! HTTP handler functions.

use axum::extract::{Path, State};
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
        .route("/shutdown", post(shutdown))
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
        Ok(EngineResponse::Error(e)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
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
                .filter(|d| d.online && d.process_running)
                .count();
            let ready = total > 0 && online == total;
            Json(serde_json::json!({
                "ready": ready,
                "online": online,
                "total": total,
            }))
            .into_response()
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn get_mask(State(handle): State<EngineHandle>) -> impl IntoResponse {
    match engine_request(handle, EngineCommand::GetMask).await {
        Ok(EngineResponse::Mask { value, hex, layout }) => {
            Json(serde_json::json!({
                "value": value,
                "hex": hex,
                "layout": layout,
            }))
            .into_response()
        }
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
