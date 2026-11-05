//! Track loading, decoding, node info / stats and cache administration endpoints.

use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Query, State},
    Json,
};
use serde_json::{json, Value};

use super::{
    models::{Git, Info, LoadResult, Plugin, Stats, Track, Version},
    track_codec, ApiError, AppState,
};

const LAVALINK_API_VERSION: &str = "4.1.1";

pub async fn version() -> &'static str {
    LAVALINK_API_VERSION
}

pub async fn info(State(ctx): State<AppState>) -> Json<Info> {
    let mut parts = LAVALINK_API_VERSION.split('.').map(|p| p.parse().unwrap_or(0));
    Json(Info {
        version: Version {
            semver: LAVALINK_API_VERSION.into(),
            major: parts.next().unwrap_or(4),
            minor: parts.next().unwrap_or(0),
            patch: parts.next().unwrap_or(0),
            pre_release: None,
            build: None,
        },
        build_time: 0,
        git: Git {
            branch: "main".into(),
            commit: "unknown".into(),
            commit_time: 0,
        },
        jvm: "none (rust)".into(),
        lavaplayer: concat!("rust-link ", env!("CARGO_PKG_VERSION")).into(),
        source_managers: ctx.sources.source_names(),
        filters: ctx.config.lavalink.filters.enabled_names(),
        plugins: vec![Plugin {
            name: "rust-link-cache".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        }],
    })
}

pub async fn stats(State(ctx): State<AppState>) -> Json<Stats> {
    Json(ctx.stats(None))
}

pub async fn load_tracks(
    State(ctx): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<LoadResult>, ApiError> {
    let identifier = query
        .get("identifier")
        .ok_or_else(|| ApiError::bad_request("Missing identifier query parameter"))?;
    let result = ctx.sources.load_tracks(identifier).await;
    if let LoadResult::Playlist(playlist) = &result {
        let start = usize::try_from(playlist.info.selected_track).unwrap_or(0);
        let tracks = playlist.tracks.iter().map(|t| t.info.clone()).collect();
        ctx.cache.remember_playlist(tracks, start);
    }
    Ok(Json(result))
}

fn decode_one(encoded: &str) -> Result<Track, ApiError> {
    let info =
        track_codec::decode_track(encoded).map_err(|e| ApiError::bad_request(format!("Invalid encoded track: {e}")))?;
    Ok(Track {
        encoded: encoded.to_string(),
        info,
        plugin_info: Default::default(),
        user_data: Default::default(),
    })
}

pub async fn decode_track(Query(query): Query<HashMap<String, String>>) -> Result<Json<Track>, ApiError> {
    let encoded = query
        .get("encodedTrack")
        .or_else(|| query.get("track"))
        .ok_or_else(|| ApiError::bad_request("Missing encodedTrack query parameter"))?;
    Ok(Json(decode_one(encoded)?))
}

pub async fn decode_tracks(body: Bytes) -> Result<Json<Vec<Track>>, ApiError> {
    let encoded: Vec<String> =
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(format!("Invalid request body: {e}")))?;
    encoded
        .iter()
        .map(|e| decode_one(e))
        .collect::<Result<Vec<_>, _>>()
        .map(Json)
}

pub async fn cache_summary(State(ctx): State<AppState>) -> Result<Json<Value>, ApiError> {
    let summary = ctx
        .cache
        .index()
        .summary()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let cfg = ctx.cache.config();
    Ok(Json(json!({
        "summary": summary,
        "maxSize": cfg.max_size.as_u64(),
        "highWatermark": cfg.high_watermark,
        "lowWatermark": cfg.low_watermark,
        "onedrive": ctx.cache.onedrive().is_some(),
    })))
}

pub async fn cache_evict(State(ctx): State<AppState>) -> Result<Json<Value>, ApiError> {
    let report = ctx
        .cache
        .enforce_capacity()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(serde_json::to_value(report).unwrap_or_default()))
}

pub async fn cache_sync(State(ctx): State<AppState>) -> Result<Json<Value>, ApiError> {
    if ctx.cache.onedrive().is_none() {
        return Err(ApiError::bad_request("OneDrive backup is not enabled"));
    }
    ctx.cache.sync_notify().notify_one();
    Ok(Json(json!({"scheduled": true})))
}
