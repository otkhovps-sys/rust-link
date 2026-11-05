//! Lavalink v4 wire types (REST + WebSocket).

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

pub use crate::audio::filters::Filters;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub identifier: String,
    pub is_seekable: bool,
    pub author: String,
    pub length: u64,
    pub is_stream: bool,
    pub position: u64,
    pub title: String,
    pub uri: Option<String>,
    pub artwork_url: Option<String>,
    pub isrc: Option<String>,
    pub source_name: String,
}

/// Length reported by Lavalink for live streams (`Long.MAX_VALUE`).
pub const STREAM_LENGTH: u64 = i64::MAX as u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub encoded: String,
    pub info: TrackInfo,
    #[serde(default)]
    pub plugin_info: Map<String, Value>,
    #[serde(default)]
    pub user_data: Map<String, Value>,
}

impl Track {
    pub fn new(info: TrackInfo) -> Self {
        Self::with_plugin_info(info, Map::new())
    }

    pub fn with_plugin_info(info: TrackInfo, plugin_info: Map<String, Value>) -> Self {
        Self {
            encoded: crate::api::track_codec::encode_track_with_plugin_info(&info, &plugin_info),
            info,
            plugin_info,
            user_data: Map::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistInfo {
    pub name: String,
    pub selected_track: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistData {
    pub info: PlaylistInfo,
    pub plugin_info: Map<String, Value>,
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmptyData {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Severity {
    Common,
    Suspicious,
    Fault,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Exception {
    pub message: Option<String>,
    pub severity: Severity,
    pub cause: String,
    #[serde(default)]
    pub cause_stack_trace: String,
}

impl Exception {
    pub fn new(message: impl Into<String>, severity: Severity) -> Self {
        let message = message.into();
        Self {
            cause: message.clone(),
            cause_stack_trace: String::new(),
            message: Some(message),
            severity,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "loadType", content = "data", rename_all = "camelCase")]
pub enum LoadResult {
    Track(Track),
    Playlist(PlaylistData),
    Search(Vec<Track>),
    Empty(EmptyData),
    Error(Exception),
}

impl LoadResult {
    pub fn empty() -> Self {
        LoadResult::Empty(EmptyData {})
    }

    pub fn error(message: impl Into<String>, severity: Severity) -> Self {
        LoadResult::Error(Exception::new(message, severity))
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerState {
    pub time: u64,
    pub position: u64,
    pub connected: bool,
    pub ping: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceState {
    pub token: String,
    pub endpoint: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Player {
    pub guild_id: String,
    pub track: Option<Track>,
    pub volume: u16,
    pub paused: bool,
    pub state: PlayerState,
    pub voice: VoiceState,
    pub filters: Filters,
}

/// Deserializes `null` as `Some(None)` and a missing field (with `#[serde(default)]`) as `None`.
pub fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePlayerTrack {
    #[serde(default, deserialize_with = "double_option")]
    pub encoded: Option<Option<String>>,
    pub identifier: Option<String>,
    pub user_data: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePlayer {
    pub track: Option<UpdatePlayerTrack>,
    /// Deprecated in v4 but still accepted by Lavalink.
    #[serde(default, deserialize_with = "double_option")]
    pub encoded_track: Option<Option<String>>,
    /// Deprecated in v4 but still accepted by Lavalink.
    pub identifier: Option<String>,
    pub position: Option<u64>,
    #[serde(default, deserialize_with = "double_option")]
    pub end_time: Option<Option<u64>>,
    pub volume: Option<i64>,
    pub paused: Option<bool>,
    pub filters: Option<Value>,
    pub voice: Option<VoiceState>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSession {
    pub resuming: Option<bool>,
    pub timeout: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub resuming: bool,
    pub timeout: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TrackEndReason {
    Finished,
    LoadFailed,
    Stopped,
    Replaced,
    Cleanup,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
#[allow(clippy::enum_variant_names)]
pub enum Event {
    TrackStartEvent {
        guild_id: String,
        track: Track,
    },
    TrackEndEvent {
        guild_id: String,
        track: Track,
        reason: TrackEndReason,
    },
    TrackExceptionEvent {
        guild_id: String,
        track: Track,
        exception: Exception,
    },
    TrackStuckEvent {
        guild_id: String,
        track: Track,
        threshold_ms: u64,
    },
    WebSocketClosedEvent {
        guild_id: String,
        code: u16,
        reason: String,
        by_remote: bool,
    },
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    pub free: u64,
    pub used: u64,
    pub allocated: u64,
    pub reservable: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cpu {
    pub cores: u32,
    pub system_load: f64,
    pub lavalink_load: f64,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameStats {
    pub sent: i64,
    pub nulled: i64,
    pub deficit: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub players: u32,
    pub playing_players: u32,
    pub uptime: u64,
    pub memory: Memory,
    pub cpu: Cpu,
    pub frame_stats: Option<FrameStats>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[allow(clippy::large_enum_variant)]
pub enum OutgoingMessage {
    Ready { resumed: bool, session_id: String },
    PlayerUpdate { guild_id: String, state: PlayerState },
    Stats(Stats),
    Event(Event),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Version {
    pub semver: String,
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub pre_release: Option<String>,
    pub build: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Git {
    pub branch: String,
    pub commit: String,
    pub commit_time: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plugin {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub version: Version,
    pub build_time: i64,
    pub git: Git,
    pub jvm: String,
    pub lavaplayer: String,
    pub source_managers: Vec<String>,
    pub filters: Vec<String>,
    pub plugins: Vec<Plugin>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_track() -> Track {
        Track::new(TrackInfo {
            identifier: "dQw4w9WgXcQ".into(),
            is_seekable: true,
            author: "RickAstleyVEVO".into(),
            length: 212_000,
            is_stream: false,
            position: 0,
            title: "Never Gonna Give You Up".into(),
            uri: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ".into()),
            artwork_url: None,
            isrc: None,
            source_name: "youtube".into(),
        })
    }

    #[test]
    fn ready_message_shape() {
        let msg = OutgoingMessage::Ready {
            resumed: false,
            session_id: "abc".into(),
        };
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            json!({"op": "ready", "resumed": false, "sessionId": "abc"})
        );
    }

    #[test]
    fn event_message_shape() {
        let msg = OutgoingMessage::Event(Event::TrackEndEvent {
            guild_id: "1".into(),
            track: sample_track(),
            reason: TrackEndReason::LoadFailed,
        });
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["op"], "event");
        assert_eq!(v["type"], "TrackEndEvent");
        assert_eq!(v["guildId"], "1");
        assert_eq!(v["reason"], "loadFailed");
        assert_eq!(v["track"]["info"]["sourceName"], "youtube");
    }

    #[test]
    fn stats_message_is_flat() {
        let msg = OutgoingMessage::Stats(Stats::default());
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["op"], "stats");
        assert_eq!(v["playingPlayers"], 0);
        assert!(v["frameStats"].is_null());
    }

    #[test]
    fn load_result_shapes() {
        assert_eq!(
            serde_json::to_value(LoadResult::empty()).unwrap(),
            json!({"loadType": "empty", "data": {}})
        );
        let v = serde_json::to_value(LoadResult::Search(vec![sample_track()])).unwrap();
        assert_eq!(v["loadType"], "search");
        assert_eq!(v["data"][0]["info"]["identifier"], "dQw4w9WgXcQ");
    }

    #[test]
    fn update_player_distinguishes_null_and_missing() {
        let stop: UpdatePlayer = serde_json::from_value(json!({"track": {"encoded": null}})).unwrap();
        assert_eq!(stop.track.unwrap().encoded, Some(None));
        let untouched: UpdatePlayer = serde_json::from_value(json!({"volume": 50})).unwrap();
        assert!(untouched.track.is_none());
        assert!(untouched.end_time.is_none());
        let reset_end: UpdatePlayer = serde_json::from_value(json!({"endTime": null})).unwrap();
        assert_eq!(reset_end.end_time, Some(None));
    }
}
