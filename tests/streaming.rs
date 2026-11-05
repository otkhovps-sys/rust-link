//! Playback without the cache: long files over HTTP ranges, and HLS (VOD and live).

mod common;

use std::{path::Path, process::Command, time::Duration};

use common::{
    media_server,
    voice_mock::{lavalink_ws, opus_samples, start_voice_server, wait_event},
    write_wav, TestNode,
};
use rust_link::{api::models::STREAM_LENGTH, audio::crypto::EncryptionMode};
use serde_json::{json, Value};
use tokio::sync::mpsc;

const SILENCE: [u8; 3] = [0xF8, 0xFF, 0xFE];

async fn load(node: &TestNode, identifier: &str) -> Value {
    node.get("/v4/loadtracks")
        .query(&[("identifier", identifier)])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn play(node: &TestNode, session_id: &str, guild: u64, endpoint: &str, encoded: &str) {
    let response = node
        .patch(&format!("/v4/sessions/{session_id}/players/{guild}"))
        .json(&json!({
            "voice": {"token": "t", "endpoint": endpoint, "sessionId": "s", "channelId": "1"},
            "track": {"encoded": encoded}
        }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.text().await.unwrap());
}

/// Audio frames received until the stream goes quiet for 500 ms.
async fn drain_audio(frames: &mut mpsc::UnboundedReceiver<Vec<u8>>) -> usize {
    let mut audio = 0;
    while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_millis(500), frames.recv()).await {
        if frame.as_slice() != SILENCE {
            assert_eq!(opus_samples(&frame), 960);
            audio += 1;
        }
    }
    audio
}

fn ffmpeg_hls(dir: &Path, seconds: u32, playlist: &str) -> bool {
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("sine=frequency=440:sample_rate=48000:duration={seconds}"))
        .args(["-ac", "2", "-c:a", "aac", "-b:a", "96k", "-f", "hls", "-hls_time", "1"])
        .args([
            "-hls_playlist_type",
            "vod",
            "-hls_segment_type",
            "mpegts",
            "-hls_segment_filename",
        ])
        .arg(dir.join("seg%d.ts"))
        .arg(dir.join(playlist))
        .status();
    match status {
        Ok(status) if status.success() => true,
        _ => {
            eprintln!("ffmpeg with AAC/HLS support not available, skipping");
            false
        }
    }
}

#[tokio::test]
async fn large_http_file_streams_with_ranges_and_seeks() {
    let media = tempfile::tempdir().unwrap();
    // 12 s of 48 kHz stereo PCM = 2.3 MB: three 1 MiB range chunks.
    write_wav(&media.path().join("long.wav"), 48_000, 2, 12.0);
    let server = media_server::serve(media.path().to_path_buf(), 0).await;
    let node = TestNode::start(|c| c.cache.max_file_size = "100KiB".parse().unwrap()).await;
    let (endpoint, _log, mut frames) = start_voice_server(EncryptionMode::AeadAes256GcmRtpSize).await;
    let (mut events, session_id) = lavalink_ws(&node).await;

    let result = load(&node, &format!("{}/long.wav", server.base)).await;
    assert_eq!(result["loadType"], "track", "{result}");
    let encoded = result["data"]["encoded"].as_str().unwrap().to_string();
    play(&node, &session_id, 77, &endpoint, &encoded).await;
    wait_event(&mut events, "TrackStartEvent").await;

    tokio::time::sleep(Duration::from_millis(600)).await;
    let seek = node
        .patch(&format!("/v4/sessions/{session_id}/players/77"))
        .json(&json!({"position": 10_000}))
        .send()
        .await
        .unwrap();
    assert!(seek.status().is_success());
    let end = wait_event(&mut events, "TrackEndEvent").await;
    assert_eq!(end["reason"], "finished");

    let audio = drain_audio(&mut frames).await;
    // ~0.6 s before the seek, then the last 2 s: far less than the full 600 frames.
    assert!((100..=220).contains(&audio), "got {audio} audio frames");

    let ranges = server.ranges_for("long.wav");
    assert!(ranges.iter().any(|r| r == "bytes=0-1048575"), "{ranges:?}");
    assert!(
        ranges.iter().any(|r| r.starts_with("bytes=1048576-")),
        "seeking to 10 s reads the second chunk: {ranges:?}"
    );
    let summary: Value = node
        .get("/v4/rustlink/cache")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(summary["summary"]["entries"], 0, "too large to cache: nothing stored");
}

#[tokio::test]
async fn hls_vod_playlist_plays_from_ts_segments() {
    let media = tempfile::tempdir().unwrap();
    if !ffmpeg_hls(media.path(), 4, "vod.m3u8") {
        return;
    }
    let server = media_server::serve(media.path().to_path_buf(), 0).await;
    let node = TestNode::start(|_| {}).await;
    let (endpoint, _log, mut frames) = start_voice_server(EncryptionMode::AeadAes256GcmRtpSize).await;
    let (mut events, session_id) = lavalink_ws(&node).await;

    let result = load(&node, &format!("{}/vod.m3u8", server.base)).await;
    assert_eq!(result["loadType"], "track", "{result}");
    let info = &result["data"]["info"];
    assert_eq!(info["isStream"], false);
    let length = info["length"].as_u64().unwrap();
    assert!((3_800..=4_300).contains(&length), "length {length}");

    play(
        &node,
        &session_id,
        78,
        &endpoint,
        result["data"]["encoded"].as_str().unwrap(),
    )
    .await;
    wait_event(&mut events, "TrackStartEvent").await;
    let end = wait_event(&mut events, "TrackEndEvent").await;
    assert_eq!(end["reason"], "finished");
    let audio = drain_audio(&mut frames).await;
    assert!((185..=215).contains(&audio), "4 s of audio, got {audio} frames");
}

#[tokio::test]
async fn hls_live_window_plays_continuously() {
    let media = tempfile::tempdir().unwrap();
    if !ffmpeg_hls(media.path(), 12, "unused.m3u8") {
        return;
    }
    let server = media_server::serve(media.path().to_path_buf(), 12).await;
    let node = TestNode::start(|_| {}).await;
    let (endpoint, _log, mut frames) = start_voice_server(EncryptionMode::AeadAes256GcmRtpSize).await;
    let (mut events, session_id) = lavalink_ws(&node).await;

    let result = load(&node, &format!("{}/live.m3u8", server.base)).await;
    assert_eq!(result["loadType"], "track", "{result}");
    assert_eq!(result["data"]["info"]["isStream"], true);
    assert_eq!(result["data"]["info"]["length"], STREAM_LENGTH);

    play(
        &node,
        &session_id,
        79,
        &endpoint,
        result["data"]["encoded"].as_str().unwrap(),
    )
    .await;
    wait_event(&mut events, "TrackStartEvent").await;

    let mut audio = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, frames.recv()).await {
        if frame.as_slice() != SILENCE {
            audio += 1;
        }
    }
    // Real-time pacing: about 50 frames per second once the first segments arrived.
    assert!(audio >= 180, "live audio kept flowing: {audio} frames in 5 s");
    let playlist_polls = server
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.path == "live.m3u8")
        .count();
    assert!(
        playlist_polls >= 3,
        "the live playlist is refreshed ({playlist_polls} loads)"
    );
    assert!(events
        .try_recv()
        .map(|e| e["type"] != "TrackExceptionEvent")
        .unwrap_or(true));

    let deleted = node
        .delete(&format!("/v4/sessions/{session_id}/players/79"))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 204);
}
