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
    // The event buffered while detached is replayed right after ready.
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
