//! Session and player REST endpoints.

use std::{collections::HashMap, sync::Arc};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use super::{
    models::{Player, SessionInfo, UpdatePlayer, UpdateSession},
    ApiError, AppState,
};
use crate::player::Session;

fn session(ctx: &AppState, id: &str) -> Result<Arc<Session>, ApiError> {
    ctx.sessions
        .get(id)
        .ok_or_else(|| ApiError::not_found("Session not found"))
}

fn guild(id: &str) -> Result<u64, ApiError> {
    id.parse().map_err(|_| ApiError::bad_request("Invalid guild id"))
}

fn json_body<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("Invalid request body: {e}")))
}

pub async fn get_players(
    State(ctx): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<Vec<Player>>, ApiError> {
    let session = session(&ctx, &session_id)?;
    let mut players = Vec::new();
    for player in session.players() {
        players.push(player.to_model().await);
    }
    Ok(Json(players))
}

pub async fn get_player(
    State(ctx): State<AppState>,
    Path((session_id, guild_id)): Path<(String, String)>,
) -> Result<Json<Player>, ApiError> {
    let session = session(&ctx, &session_id)?;
    let player = session
        .player(guild(&guild_id)?)
        .ok_or_else(|| ApiError::not_found("Player not found"))?;
    Ok(Json(player.to_model().await))
}

pub async fn update_player(
    State(ctx): State<AppState>,
    Path((session_id, guild_id)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Player>, ApiError> {
    let session = session(&ctx, &session_id)?;
    let guild_id = guild(&guild_id)?;
    let request: UpdatePlayer = json_body(&body)?;
    let no_replace = query.get("noReplace").is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let player = session.get_or_create_player(guild_id, &ctx);
    Ok(Json(player.update(request, no_replace).await?))
}

pub async fn destroy_player(
    State(ctx): State<AppState>,
    Path((session_id, guild_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let session = session(&ctx, &session_id)?;
    session.destroy_player(guild(&guild_id)?).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn update_session(
    State(ctx): State<AppState>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Json<SessionInfo>, ApiError> {
    let session = session(&ctx, &session_id)?;
    let request: UpdateSession = json_body(&body)?;
    Ok(Json(session.update(request)))
}
