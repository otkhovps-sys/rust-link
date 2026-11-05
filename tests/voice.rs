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
