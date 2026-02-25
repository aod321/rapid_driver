//! HTTP REST API server using axum.

mod routes;

use axum::Router;
use tower_http::cors::{Any, CorsLayer};

use crate::engine::command::EngineHandle;

/// Build the axum router with all API routes.
pub fn build_router(handle: EngineHandle) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    routes::router().layer(cors).with_state(handle)
}
