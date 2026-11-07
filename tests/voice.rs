//! End to end: Lavalink client -> REST play -> cache -> decoder -> Opus -> RTP/AEAD -> UDP,
//! against a mock Discord voice server (gateway v8 + UDP IP discovery).

mod common;

use std::time::Duration;

use common::{
    voice_mock::{lavalink_ws, opus_samples, start_voice_server, wait_event},
    TestNode,
};
use rust_link::audio::crypto::EncryptionMode;
use serde_json::{json, Value};

async fn play_through_mock(mode: EncryptionMode, configured: &str) {
    let configured = configured.to_string();
    let node = TestNode::start(move |c| c.audio.encryption_mode = configured).await;
    let (_info, encoded) = node.seed_cached_wav("e2e tone", 1.5).await;
    let (endpoint, log, mut frames) = start_voice_server(mode).await;
    let (mut events, session_id) = lavalink_ws(&node).await;

    let player: Value = node
        .patch(&format!("/v4/sessions/{session_id}/players/999"))
        .json(&json!({
            "voice": {"token": "voice-token", "endpoint": endpoint, "sessionId": "voice-session", "channelId": "555"},
            "track": {"encoded": encoded, "userData": {"requester": "tests"}},
            "volume": 80
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(player["track"]["info"]["title"], "e2e tone");
    assert_eq!(player["track"]["userData"]["requester"], "tests");

    let start = wait_event(&mut events, "TrackStartEvent").await;
    assert_eq!(start["guildId"], "999");
    let end = wait_event(&mut events, "TrackEndEvent").await;
    assert_eq!(end["reason"], "finished");

    let mut packets = Vec::new();
    while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_millis(500), frames.recv()).await {
        packets.push(frame);
    }
    // 1.5 s of audio = 75 frames of 20 ms, followed by Opus silence frames.
    let audio: Vec<_> = packets.iter().filter(|p| p.as_slice() != [0xF8, 0xFF, 0xFE]).collect();
    assert!((74..=76).contains(&audio.len()), "got {} audio frames", audio.len());
    assert!(audio.iter().all(|p| opus_samples(p) == 960));
    assert!(packets.len() > audio.len(), "silence frames follow the audio");

    {
        let log = log.lock().await;
        let identify = log.identify.as_ref().unwrap();
        assert_eq!(identify["server_id"], "999");
        assert_eq!(identify["user_id"], "111");
        assert_eq!(identify["session_id"], "voice-session");
        assert_eq!(identify["token"], "voice-token");
        let expected_dave = if cfg!(feature = "dave") { 1 } else { 0 };
        assert_eq!(identify["max_dave_protocol_version"], expected_dave);
        let select = log.select_protocol.as_ref().unwrap();
        assert_eq!(select["protocol"], "udp");
        assert_eq!(select["data"]["mode"], mode.as_str());
        assert_eq!(select["data"]["address"], "127.0.0.1");
        assert_eq!(log.speaking.first(), Some(&1));
        assert_eq!(log.speaking.last(), Some(&0));
        assert!(log.heartbeats >= 1);
    }

    let state: Value = node
        .get(&format!("/v4/sessions/{session_id}/players/999"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["state"]["connected"], true);
    assert!(state["track"].is_null());
    assert!(state["state"]["position"].as_u64().unwrap() >= 1400);

    let deleted = node
        .delete(&format!("/v4/sessions/{session_id}/players/999"))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 204);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        log.lock().await.closed,
        "destroying the player closes the voice websocket"
    );
}

#[tokio::test]
async fn plays_cached_track_with_aes_gcm() {
    play_through_mock(EncryptionMode::AeadAes256GcmRtpSize, "auto").await;
}

#[tokio::test]
async fn plays_cached_track_with_xchacha20() {
    play_through_mock(
        EncryptionMode::AeadXChaCha20Poly1305RtpSize,
        "aead_xchacha20_poly1305_rtpsize",
    )
    .await;
}

#[tokio::test]
async fn timescale_speeds_up_playback_and_position() {
    let node = TestNode::start(|_| {}).await;
    let (_info, encoded) = node.seed_cached_wav("fast tone", 1.5).await;
    let (endpoint, _log, mut frames) = start_voice_server(EncryptionMode::AeadAes256GcmRtpSize).await;
    let (mut events, session_id) = lavalink_ws(&node).await;

    let response = node
        .patch(&format!("/v4/sessions/{session_id}/players/998"))
        .json(&json!({
            "voice": {"token": "voice-token", "endpoint": endpoint, "sessionId": "voice-session", "channelId": "555"},
            "track": {"encoded": encoded},
            "filters": {"timescale": {"speed": 1.5}, "equalizer": [{"band": 2, "gain": 0.2}]}
        }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    wait_event(&mut events, "TrackStartEvent").await;
    let end = wait_event(&mut events, "TrackEndEvent").await;
    assert_eq!(end["reason"], "finished");

    let mut audio = 0;
    while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_millis(500), frames.recv()).await {
        if frame.as_slice() != [0xF8, 0xFF, 0xFE] {
            assert_eq!(opus_samples(&frame), 960);
            audio += 1;
        }
    }
    // 1.5 s of audio at 1.5x speed is played in about 1 s (50 frames).
    assert!((47..=53).contains(&audio), "got {audio} audio frames");

    let state: Value = node
        .get(&format!("/v4/sessions/{session_id}/players/998"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let position = state["state"]["position"].as_u64().unwrap();
    assert!(
        (1400..=1600).contains(&position),
        "position follows the track, not the clock: {position}"
    );
}

/// RMS of a decoded Opus packet.
fn loudness(decoder: &mut audiopus::coder::Decoder, packet: &[u8]) -> f32 {
    let mut pcm = vec![0f32; 5_760 * 2];
    let packet = audiopus::packet::Packet::try_from(packet).unwrap();
    let output = audiopus::MutSignals::try_from(&mut pcm[..]).unwrap();
    let samples = decoder.decode_float(Some(packet), output, false).unwrap() * 2;
    (pcm[..samples].iter().map(|s| s * s).sum::<f32>() / samples.max(1) as f32).sqrt()
}

#[tokio::test]
async fn volume_and_filter_changes_are_heard_immediately() {
    let node = TestNode::start(|c| c.audio.frame_buffer_ms = 400).await;
    let (_info, encoded) = node.seed_cached_wav("instant tone", 4.0).await;
    let (endpoint, _log, mut frames) = start_voice_server(EncryptionMode::AeadAes256GcmRtpSize).await;
    let (mut events, session_id) = lavalink_ws(&node).await;
    let path = format!("/v4/sessions/{session_id}/players/997");
    let response = node
        .patch(&path)
        .json(&json!({
            "voice": {"token": "t", "endpoint": endpoint, "sessionId": "s", "channelId": "1"},
            "track": {"encoded": encoded}
        }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    wait_event(&mut events, "TrackStartEvent").await;

    let mut decoder = audiopus::coder::Decoder::new(audiopus::SampleRate::Hz48000, audiopus::Channels::Stereo).unwrap();
    // (time received, loudness) of every frame.
    let mut received = Vec::new();
    async fn record(
        frames: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        decoder: &mut audiopus::coder::Decoder,
        out: &mut Vec<(tokio::time::Instant, f32)>,
        ms: u64,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
        while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, frames.recv()).await {
            if frame.as_slice() != [0xF8, 0xFF, 0xFE] {
                out.push((tokio::time::Instant::now(), loudness(decoder, &frame)));
            }
        }
    }

    record(&mut frames, &mut decoder, &mut received, 900).await;
    let loud = received.iter().map(|r| r.1).fold(0.0, f32::max);
    assert!(loud > 0.1, "tone is audible before the change: {loud}");

    // Each change must be audible within 150 ms, not after the 400 ms frame buffer.
    for (body, quiet_after) in [
        (json!({"volume": 0}), true),
        (json!({"volume": 100}), false),
        (json!({"filters": {"volume": 0.0}}), true),
    ] {
        let changed_at = tokio::time::Instant::now();
        let response = node.patch(&path).json(&body).send().await.unwrap();
        assert!(response.status().is_success());
        let start = received.len();
        record(&mut frames, &mut decoder, &mut received, 600).await;
        let late: Vec<f32> = received[start..]
            .iter()
            .filter(|(at, _)| at.duration_since(changed_at) > Duration::from_millis(150))
            .map(|(_, level)| *level)
            .collect();
        assert!(late.len() > 10, "{body}: frames kept flowing ({})", late.len());
        let wrong = late
            .iter()
            .filter(|level| {
                if quiet_after {
                    **level > loud * 0.05
                } else {
                    **level < loud * 0.5
                }
            })
            .count();
        assert_eq!(
            wrong, 0,
            "{body}: {wrong} frames still had the old level 150 ms after the change: {late:?}"
        );
    }
}
