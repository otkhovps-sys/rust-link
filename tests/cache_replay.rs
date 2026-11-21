//! End to end: a bot searches a YouTube track, plays it into a voice channel, then plays
//! it again. The first play downloads through yt-dlp and stores the file; the second is
//! served from the cache without starting yt-dlp, and the audio still reaches Discord.
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::{
    fake_ytdlp::FakeYtDlp,
    voice_mock::{lavalink_ws, start_voice_server, wait_event},
    TestNode,
};
use rust_link::audio::crypto::EncryptionMode;
use serde_json::{json, Value};

const ONE_RESULT: &str = r#"{"_type":"playlist","entries":[{"_type":"url","ie_key":"Youtube","id":"ccccccccccc","title":"Cached song","duration":2.0,"channel":"z"}]}"#;

/// Opus silence frame sent after a track ends.
const SILENCE: [u8; 3] = [0xF8, 0xFF, 0xFE];

async fn audio_frames(frames: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) -> usize {
    let mut audio = 0;
    while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_millis(500), frames.recv()).await {
        if frame.as_slice() != SILENCE {
            audio += 1;
        }
    }
    audio
}

async fn cache_entries(node: &TestNode) -> u64 {
    let summary: Value = node
        .get("/v4/rustlink/cache")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    summary["summary"]["entries"].as_u64().unwrap()
}

#[tokio::test]
async fn second_play_comes_from_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let ytdlp = FakeYtDlp::install(dir.path(), ONE_RESULT, 2.0);
    let path = ytdlp.path.clone();
    let node = TestNode::start(move |c| {
        c.sources.ytdlp.path = path;
        c.audio.encryption_mode = "auto".into();
    })
    .await;

    let found: Value = node
        .get("/v4/loadtracks")
        .query(&[("identifier", "ytsearch:cached song")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found["loadType"], "search", "{found}");
    let encoded = found["data"][0]["encoded"].as_str().unwrap().to_string();
    assert_eq!(cache_entries(&node).await, 0, "nothing cached before the first play");

    let (endpoint, _log, mut frames) = start_voice_server(EncryptionMode::AeadAes256GcmRtpSize).await;
    let (mut events, session_id) = lavalink_ws(&node).await;
    let play = |encoded: String| {
        node.patch(&format!("/v4/sessions/{session_id}/players/42"))
            .json(&json!({
                "voice": {"token": "t", "endpoint": endpoint, "sessionId": "s", "channelId": "1"},
                "track": {"encoded": encoded}
            }))
            .send()
    };

    // First play: yt-dlp downloads the audio, the node stores it while playing.
    assert!(play(encoded.clone()).await.unwrap().status().is_success());
    wait_event(&mut events, "TrackStartEvent").await;
    assert_eq!(wait_event(&mut events, "TrackEndEvent").await["reason"], "finished");
    let first = audio_frames(&mut frames).await;
    assert!((99..=101).contains(&first), "first play: {first} audio frames");
    assert_eq!(ytdlp.downloads().len(), 1, "{:?}", ytdlp.calls());
    assert_eq!(cache_entries(&node).await, 1, "track stored after the first play");

    // Second play of the same track: served from the cache, yt-dlp is not started.
    let calls_before = ytdlp.calls().len();
    assert!(play(encoded).await.unwrap().status().is_success());
    wait_event(&mut events, "TrackStartEvent").await;
    assert_eq!(wait_event(&mut events, "TrackEndEvent").await["reason"], "finished");
    let second = audio_frames(&mut frames).await;
    assert!((99..=101).contains(&second), "second play: {second} audio frames");
    assert_eq!(
        ytdlp.calls().len(),
        calls_before,
        "yt-dlp started again: {:?}",
        ytdlp.calls()
    );
    assert_eq!(cache_entries(&node).await, 1);
}
