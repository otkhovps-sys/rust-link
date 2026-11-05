//! Lavalink v4 REST + websocket surface.

mod common;

use std::time::Duration;

use common::{TestNode, PASSWORD};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

async fn connect_ws(
    node: &TestNode,
    session_id: Option<&str>,
) -> (
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    bool,
) {
    let mut request = format!("ws://{}/v4/websocket", node.addr)
        .into_client_request()
        .unwrap();
    let headers = request.headers_mut();
    headers.insert("Authorization", PASSWORD.parse().unwrap());
    headers.insert("User-Id", "123456789".parse().unwrap());
    headers.insert("Client-Name", "tests/1.0".parse().unwrap());
    if let Some(id) = session_id {
        headers.insert("Session-Id", id.parse().unwrap());
    }
    let (ws, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    let resumed = response.headers().get("Session-Resumed").and_then(|v| v.to_str().ok()) == Some("true");
    (ws, resumed)
}

async fn next_json<S>(ws: &mut S) -> Value
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("websocket message")
            .unwrap()
            .unwrap();
        if let Message::Text(text) = msg {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

#[tokio::test]
async fn auth_errors_use_lavalink_format() {
    let node = TestNode::start(|_| {}).await;
    let response = node.http.get(node.url("/v4/info")).send().await.unwrap();
    assert_eq!(response.status(), 401);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["status"], 401);
    assert_eq!(body["error"], "Unauthorized");
    assert_eq!(body["path"], "/v4/info");
    assert!(body["timestamp"].as_i64().unwrap() > 0);

    let version = node.get("/version").send().await.unwrap().text().await.unwrap();
    assert!(version.starts_with('4'));

    let info: Value = node.get("/v4/info").send().await.unwrap().json().await.unwrap();
    assert_eq!(info["version"]["major"], 4);
    assert!(info["sourceManagers"].as_array().unwrap().contains(&json!("youtube")));

    let missing: Value = node
        .get("/v4/sessions/nope/players")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(missing["status"], 404);
    assert_eq!(missing["message"], "Session not found");

    let unknown = node.get("/v5/whatever").send().await.unwrap();
    assert_eq!(unknown.status(), 404);
}

#[tokio::test]
async fn loadtracks_and_decode_from_cache_index() {
    let node = TestNode::start(|_| {}).await;
    let (info, encoded) = node.seed_cached_wav("cached tone", 1.0).await;

    let result: Value = node
        .get("/v4/loadtracks?identifier=cachesearch:cached")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["loadType"], "search");
    assert_eq!(result["data"][0]["info"]["title"], "cached tone");
    assert_eq!(result["data"][0]["encoded"], encoded);

    let empty: Value = node
        .get("/v4/loadtracks?identifier=plain%20text%20without%20prefix")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(empty, json!({"loadType": "empty", "data": {}}));

    let decoded: Value = node
        .get(&format!("/v4/decodetrack?encodedTrack={}", urlencode(&encoded)))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(decoded["info"]["uri"], info.uri.unwrap());
    assert_eq!(decoded["info"]["length"], 1000);

    let bad = node.get("/v4/decodetrack?encodedTrack=abc").send().await.unwrap();
    assert_eq!(bad.status(), 400);

    let summary: Value = node
        .get("/v4/rustlink/cache")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(summary["summary"]["entries"], 1);
    assert_eq!(summary["summary"]["pendingSync"], 1);
}

fn urlencode(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

#[tokio::test]
async fn track_start_has_current_player_state() {
    use common::voice_mock::lavalink_ws;

    let node = TestNode::start(|c| c.lavalink.player_update_interval = 3600).await;
    let (_, encoded) = node.seed_cached_wav("start position", 1.0).await;
    let (mut events, session_id) = lavalink_ws(&node).await;
    let started_at = rust_link::util::now_millis() as u64;
    let response = node
        .patch(&format!("/v4/sessions/{session_id}/players/42"))
        .json(&json!({"track": {"encoded": encoded}, "position": 250, "paused": true}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    let mut state = None;
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        if message["op"] == "playerUpdate" && message["guildId"] == "42" {
            state = Some(message["state"].clone());
        }
        if message["type"] == "TrackStartEvent" {
            let state = state.expect("TrackStartEvent must follow a fresh playerUpdate");
            assert_eq!(state["position"], 250);
            assert!(state["time"].as_u64().unwrap() >= started_at);
            break;
        }
    }
}

#[tokio::test]
async fn filters_are_validated_normalized_and_listed() {
    let node = TestNode::start(|c| c.lavalink.filters.karaoke = false).await;
    let info: Value = node.get("/v4/info").send().await.unwrap().json().await.unwrap();
    let names: Vec<&str> = info["filters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "volume",
            "equalizer",
            "timescale",
            "tremolo",
            "vibrato",
            "distortion",
            "rotation",
            "channelMix",
            "lowPass"
        ]
    );

    let (_events, session_id) = common::voice_mock::lavalink_ws(&node).await;
    let path = format!("/v4/sessions/{session_id}/players/7");
    let player: Value = node
        .patch(&path)
        .json(&json!({"filters": {
            "volume": 0.8,
            "equalizer": [{"band": 0, "gain": 0.2}, {"band": 14, "gain": 3}],
            "timescale": {"speed": 1.2, "pitch": 1.1},
            "tremolo": {"depth": 0.3},
            "vibrato": null,
            "rotation": {"rotationHz": 0.2},
            "distortion": {"sinScale": 2},
            "channelMix": {"leftToRight": 0.5},
            "lowPass": {"smoothing": 10},
            "pluginFilters": {"some-plugin": {"enabled": true}}
        }}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let filters = &player["filters"];
    assert_eq!(filters["volume"], 0.8);
    assert_eq!(
        filters["equalizer"],
        json!([{"band": 0, "gain": 0.2}, {"band": 14, "gain": 1.0}])
    );
    assert_eq!(filters["timescale"]["rate"], 1.0);
    assert_eq!(filters["tremolo"]["frequency"], 2.0);
    assert!(filters.get("vibrato").is_none());
    assert_eq!(filters["distortion"]["cosScale"], 1.0);
    assert_eq!(filters["channelMix"]["rightToRight"], 1.0);
    assert_eq!(filters["pluginFilters"]["some-plugin"]["enabled"], true);

    // A PATCH without `filters` keeps them; `filters: {}` clears them.
    let kept: Value = node
        .patch(&path)
        .json(&json!({"volume": 90}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(kept["filters"], player["filters"]);
    let cleared: Value = node
        .patch(&path)
        .json(&json!({"filters": {}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cleared["filters"], json!({}));

    for (body, message) in [
        (
            json!({"filters": {"karaoke": {}}}),
            "Following filters are disabled in the config: karaoke",
        ),
        (
            json!({"filters": {"timescale": {"speed": 0}}}),
            "filters.timescale.speed must be greater than 0",
        ),
        (
            json!({"filters": {"volume": 9}}),
            "filters.volume must be between 0.0 and 5.0",
        ),
    ] {
        let response = node.patch(&path).json(&body).send().await.unwrap();
        assert_eq!(response.status(), 400);
        let error: Value = response.json().await.unwrap();
        assert_eq!(error["message"], message);
    }
}

#[tokio::test]
async fn websocket_ready_players_and_resume() {
    let node = TestNode::start(|_| {}).await;
    let (_, encoded) = node.seed_cached_wav("resume tone", 1.0).await;
    let (mut ws, resumed) = connect_ws(&node, None).await;
    assert!(!resumed);
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["op"], "ready");
    assert_eq!(ready["resumed"], false);
    let session_id = ready["sessionId"].as_str().unwrap().to_string();
    assert_eq!(session_id.len(), 16);
    let stats = next_json(&mut ws).await;
    assert_eq!(stats["op"], "stats");

    // Creating a player through PATCH (no voice, no track).
    let player: Value = node
        .patch(&format!("/v4/sessions/{session_id}/players/42"))
        .json(&json!({"volume": 150, "paused": true, "filters": {"volume": 0.5}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(player["guildId"], "42");
    assert_eq!(player["volume"], 150);
    assert_eq!(player["paused"], true);
    assert_eq!(player["filters"]["volume"], 0.5);
    assert!(player["track"].is_null());

    let invalid = node
        .patch(&format!("/v4/sessions/{session_id}/players/42"))
        .json(&json!({"volume": 5000}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);

    let update = next_json(&mut ws).await;
    assert_eq!(update["op"], "playerUpdate");
    assert_eq!(update["guildId"], "42");
    assert_eq!(update["state"]["connected"], false);

    let session: Value = node
        .patch(&format!("/v4/sessions/{session_id}"))
        .json(&json!({"resuming": true, "timeout": 30}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session, json!({"resuming": true, "timeout": 30}));

    drop(ws);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let players: Value = node
        .get(&format!("/v4/sessions/{session_id}/players"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(players.as_array().unwrap().len(), 1, "players survive while resumable");

    // Events produced while the client is away are queued for the resumed connection.
    let started: Value = node
        .patch(&format!("/v4/sessions/{session_id}/players/42"))
        .json(&json!({"track": {"encoded": encoded}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(started["track"]["info"]["title"], "resume tone");

    let (mut ws, resumed) = connect_ws(&node, Some(&session_id)).await;
    assert!(resumed);
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["resumed"], true);
    assert_eq!(ready["sessionId"], session_id.as_str());
    // The initial state and start event buffered while detached are replayed in order.
    let state = next_json(&mut ws).await;
    assert_eq!(state["op"], "playerUpdate");
    assert_eq!(state["guildId"], "42");
    assert_eq!(state["state"]["position"], 0);
    let replayed = next_json(&mut ws).await;
    assert_eq!(replayed["op"], "event");
    assert_eq!(replayed["type"], "TrackStartEvent");
    assert_eq!(replayed["track"]["info"]["title"], "resume tone");

    let deleted = node
        .delete(&format!("/v4/sessions/{session_id}/players/42"))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 204);
    let gone = node
        .get(&format!("/v4/sessions/{session_id}/players/42"))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), 404);
}
