//! Live test against YouTube through a real yt-dlp binary (ignored by default).
//!
//! ```text
//! RUSTLINK_TEST_YTDLP=/usr/local/bin/yt-dlp cargo test --test live_youtube -- --ignored --nocapture
//! ```

mod common;

use std::time::Duration;

use common::{
    voice_mock::{lavalink_ws, opus_samples, start_voice_server, wait_event},
    TestNode,
};
use rust_link::audio::crypto::EncryptionMode;
use serde_json::{json, Value};

const VIDEO: &str = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";

#[tokio::test]
#[ignore = "needs network access and RUSTLINK_TEST_YTDLP"]
async fn youtube_download_progressive_play_seek_and_cache_hit() {
    let Ok(ytdlp) = std::env::var("RUSTLINK_TEST_YTDLP") else {
        eprintln!("RUSTLINK_TEST_YTDLP not set, skipping");
        return;
    };
    let node = TestNode::start(move |c| c.sources.ytdlp.path = ytdlp).await;
    let (endpoint, _log, mut frames) = start_voice_server(EncryptionMode::AeadAes256GcmRtpSize).await;
    let (mut events, session_id) = lavalink_ws(&node).await;

    let loaded: Value = node
        .get(&format!("/v4/loadtracks?identifier={VIDEO}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(loaded["loadType"], "track", "{loaded}");
    let encoded = loaded["data"]["encoded"].as_str().unwrap().to_string();
    let length = loaded["data"]["info"]["length"].as_u64().unwrap();
    assert!(length > 200_000);

    let player_path = format!("/v4/sessions/{session_id}/players/1");
    let play = json!({
        "voice": {"token": "t", "endpoint": endpoint, "sessionId": "s", "channelId": "2"},
        "track": {"encoded": encoded}
    });
    let started = std::time::Instant::now();
    node.patch(&player_path).json(&play).send().await.unwrap();
    wait_event(&mut events, "TrackStartEvent").await;

    // Progressive playback: audio flows while the download is still running.
    let mut received = 0;
    while received < 100 {
        let frame = tokio::time::timeout(Duration::from_secs(30), frames.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(frame == [0xF8, 0xFF, 0xFE] || opus_samples(&frame) == 960);
        received += 1;
    }
    eprintln!("first 2 s of audio after {:?}", started.elapsed());

    // Wait for the cache entry, then seek close to the end and let the track finish.
    let mut entry = None;
    for _ in 0..120 {
        entry = node.ctx.cache.index().get("dQw4w9WgXcQ").await.unwrap();
        if entry.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let entry = entry.expect("track cached after download");
    eprintln!(
        "cached {} bytes as {:?} at {}",
        entry.file_size, entry.container, entry.file_path
    );
    assert!(entry.file_size > 1_000_000);
    assert!(node.ctx.cache.absolute_path(&entry.file_path).exists());

    node.patch(&player_path)
        .json(&json!({"position": length - 3_000}))
        .send()
        .await
        .unwrap();
    let end = wait_event(&mut events, "TrackEndEvent").await;
    assert_eq!(end["reason"], "finished");

    // Second play is a cache hit (no yt-dlp involved).
    node.patch(&player_path)
        .json(&json!({"track": {"encoded": encoded}, "position": length - 1_000}))
        .send()
        .await
        .unwrap();
    wait_event(&mut events, "TrackStartEvent").await;
    let end = wait_event(&mut events, "TrackEndEvent").await;
    assert_eq!(end["reason"], "finished");
    let entry = node.ctx.cache.index().get("dQw4w9WgXcQ").await.unwrap().unwrap();
    assert!(entry.access_count >= 2, "access count {}", entry.access_count);

    // loadtracks is now answered from the cache index.
    let cached: Value = node
        .get("/v4/loadtracks?identifier=dQw4w9WgXcQ")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cached["data"]["info"]["identifier"], "dQw4w9WgXcQ");
}
