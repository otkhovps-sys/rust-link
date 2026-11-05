//! Lavalink v4 compatible HTTP API (REST + `/v4/websocket`).

pub mod info_routes;
pub mod models;
pub mod player_routes;
pub mod track_codec;
pub mod websocket;

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
    Json, Router,
};
use serde::Serialize;
use tower_http::trace::TraceLayer;

use crate::{node::NodeContext, util::now_millis};

pub type AppState = Arc<NodeContext>;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

/// Lavalink error body. `path` is filled in by [`error_path_layer`].
#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    pub timestamp: i64,
    pub status: u16,
    pub error: String,
    pub message: String,
    pub path: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            timestamp: now_millis(),
            status: self.status.as_u16(),
            error: self.status.canonical_reason().unwrap_or("Error").to_string(),
            message: self.message,
            path: String::new(),
        };
        let mut response = (self.status, Json(&body)).into_response();
        response.extensions_mut().insert(body);
        response
    }
}

async fn error_path_layer(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let response = next.run(request).await;
    match response.extensions().get::<ErrorBody>().cloned() {
        Some(mut body) => {
            body.path = path;
            (response.status(), Json(body)).into_response()
        }
        None => response,
    }
}

async fn auth_layer(State(ctx): State<AppState>, request: Request, next: Next) -> Response {
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| constant_time_eq(v.as_bytes(), ctx.config.server.password.as_bytes()));
    if !authorized {
        return ApiError::new(StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }
    next.run(request).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn not_found() -> ApiError {
    ApiError::not_found("Not Found")
}

pub fn router(ctx: AppState) -> Router {
    Router::new()
        .route("/version", get(info_routes::version))
        .route("/v4/info", get(info_routes::info))
        .route("/v4/stats", get(info_routes::stats))
        .route("/v4/loadtracks", get(info_routes::load_tracks))
        .route("/v4/decodetrack", get(info_routes::decode_track))
        .route("/v4/decodetracks", post(info_routes::decode_tracks))
        .route("/v4/websocket", get(websocket::websocket_handler))
        .route("/v4/sessions/{session_id}", patch(player_routes::update_session))
        .route("/v4/sessions/{session_id}/players", get(player_routes::get_players))
        .route(
            "/v4/sessions/{session_id}/players/{guild_id}",
            get(player_routes::get_player)
                .patch(player_routes::update_player)
                .delete(player_routes::destroy_player),
        )
        .route("/v4/rustlink/cache", get(info_routes::cache_summary))
        .route("/v4/rustlink/cache/evict", post(info_routes::cache_evict))
        .route("/v4/rustlink/cache/sync", post(info_routes::cache_sync))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(Arc::clone(&ctx), auth_layer))
        .layer(middleware::from_fn(error_path_layer))
        .layer(TraceLayer::new_for_http())
        .with_state(ctx)
}
