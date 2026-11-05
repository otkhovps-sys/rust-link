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

/// Audio frames received on each receiver during `ms`.
async fn count_both(
    a: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    b: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    ms: u64,
) -> (usize, usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    let (mut count_a, mut count_b) = (0, 0);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return (count_a, count_b),
            Some(frame) = a.recv() => if frame.as_slice() != SILENCE { count_a += 1 },
            Some(frame) = b.recv() => if frame.as_slice() != SILENCE { count_b += 1 },
        }
    }
}

fn most_fetches_of_a_segment(server: &media_server::MediaServer) -> usize {
    let requests = server.requests.lock().unwrap();
    let mut counts = std::collections::HashMap::new();
    for request in requests.iter().filter(|r| r.path.ends_with(".ts")) {
        *counts.entry(request.path.clone()).or_insert(0) += 1;
    }
    counts.values().copied().max().unwrap_or(0)
}

async fn two_listeners(
    share: bool,
) -> (
    TestNode,
    media_server::MediaServer,
    String,
    [mpsc::UnboundedReceiver<Vec<u8>>; 2],
) {
    let media = tempfile::tempdir().unwrap();
    assert!(ffmpeg_hls(media.path(), 12, "unused.m3u8"));
    let server = media_server::serve(media.keep(), 12).await;
    let node = TestNode::start(move |c| c.audio.share_live_streams = share).await;
    let (_events, session_id) = lavalink_ws(&node).await;
    let result = load(&node, &format!("{}/live.m3u8", server.base)).await;
    let encoded = result["data"]["encoded"].as_str().unwrap().to_string();
    let (endpoint_a, _log_a, frames_a) = start_voice_server(EncryptionMode::AeadAes256GcmRtpSize).await;
    let (endpoint_b, _log_b, frames_b) = start_voice_server(EncryptionMode::AeadAes256GcmRtpSize).await;
    play(&node, &session_id, 81, &endpoint_a, &encoded).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    play(&node, &session_id, 82, &endpoint_b, &encoded).await;
    (node, server, session_id, [frames_a, frames_b])
}

#[tokio::test]
async fn players_on_the_same_live_stream_share_one_decode() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        return;
    }
    let (node, server, session_id, [mut frames_a, mut frames_b]) = two_listeners(true).await;
    let (a, b) = count_both(&mut frames_a, &mut frames_b, 4_000).await;
    assert!(a >= 140 && b >= 140, "both servers hear the stream: {a} / {b} frames");
    let hub = node.ctx.live.as_ref().unwrap();
    assert_eq!(hub.channel_count(), 1, "one shared channel");
    assert_eq!(most_fetches_of_a_segment(&server), 1, "each segment downloaded once");

    // Another volume needs other packets: that player moves to its own channel.
    let response = node
        .patch(&format!("/v4/sessions/{session_id}/players/82"))
        .json(&json!({"volume": 50}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let (_, b_after) = count_both(&mut frames_a, &mut frames_b, 3_000).await;
    assert!(b_after >= 90, "still playing after the switch: {b_after} frames");
    assert_eq!(hub.channel_count(), 2);
}

#[tokio::test]
async fn live_sharing_can_be_disabled() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        return;
    }
    let (node, server, _session_id, [mut frames_a, mut frames_b]) = two_listeners(false).await;
    let (a, b) = count_both(&mut frames_a, &mut frames_b, 3_000).await;
    assert!(a >= 100 && b >= 100, "{a} / {b}");
    assert!(node.ctx.live.is_none());
    assert_eq!(
        most_fetches_of_a_segment(&server),
        2,
        "each player downloads on its own"
    );
}
