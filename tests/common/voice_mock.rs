//! Mock Discord voice server (gateway v8 + UDP) and Lavalink websocket helpers.

use std::{sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use rust_link::audio::crypto::{decrypt_packet, EncryptionMode};
use serde_json::{json, Value};
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::{mpsc, Mutex},
};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

use super::{TestNode, PASSWORD};

pub const SSRC: u32 = 4242;
pub const SECRET: [u8; 32] = [9; 32];

#[derive(Default)]
pub struct VoiceLog {
    pub identify: Option<Value>,
    pub select_protocol: Option<Value>,
    pub speaking: Vec<i64>,
    pub heartbeats: u32,
    pub closed: bool,
}

/// Mock voice server; returns (endpoint, log, receiver of decrypted Opus payloads).
pub async fn start_voice_server(
    mode: EncryptionMode,
) -> (String, Arc<Mutex<VoiceLog>>, mpsc::UnboundedReceiver<Vec<u8>>) {
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let udp = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let endpoint = format!("ws://{}", tcp.local_addr().unwrap());
    let udp_port = udp.local_addr().unwrap().port();
    let log = Arc::new(Mutex::new(VoiceLog::default()));
    let (frames_tx, frames_rx) = mpsc::unbounded_channel();

    let udp_rx = Arc::clone(&udp);
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            let Ok((n, from)) = udp_rx.recv_from(&mut buf).await else {
                break;
            };
            let packet = &buf[..n];
            if n == 74 && packet[1] == 1 {
                let mut reply = [0u8; 74];
                reply[1] = 2;
                reply[3] = 70;
                reply[4..8].copy_from_slice(&SSRC.to_be_bytes());
                let ip = from.ip().to_string();
                reply[8..8 + ip.len()].copy_from_slice(ip.as_bytes());
                reply[72..74].copy_from_slice(&from.port().to_be_bytes());
                udp_rx.send_to(&reply, from).await.unwrap();
            } else if n > 32 || (n > 8 && packet[0] == 0x80) {
                let (header, payload) = decrypt_packet(mode, &SECRET, packet).expect("valid RTP/AEAD packet");
                assert_eq!(header.ssrc, SSRC);
                let _ = frames_tx.send(payload);
            }
        }
    });

    let ws_log = Arc::clone(&log);
    tokio::spawn(async move {
        let (stream, _) = tcp.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let send = |v: Value| Message::text(v.to_string());
        ws.send(send(json!({"op": 8, "d": {"heartbeat_interval": 1000.0}})))
            .await
            .unwrap();
        let mut seq = 0;
        while let Some(Ok(msg)) = ws.next().await {
            let text = match msg {
                Message::Text(t) => t.to_string(),
                Message::Close(_) => {
                    ws_log.lock().await.closed = true;
                    break;
                }
                _ => continue,
            };
            let payload: Value = serde_json::from_str(&text).unwrap();
            seq += 1;
            match payload["op"].as_u64().unwrap() {
                0 => {
                    ws_log.lock().await.identify = Some(payload["d"].clone());
                    ws.send(send(json!({"op": 2, "seq": seq, "d": {
                        "ssrc": SSRC, "ip": "127.0.0.1", "port": udp_port,
                        "modes": ["aead_aes256_gcm_rtpsize", "aead_xchacha20_poly1305_rtpsize"],
                        "heartbeat_interval": 1
                    }})))
                    .await
                    .unwrap();
                }
                1 => {
                    ws_log.lock().await.select_protocol = Some(payload["d"].clone());
                    ws.send(send(json!({"op": 4, "seq": seq, "d": {
                        "mode": mode.as_str(), "secret_key": SECRET.to_vec(), "dave_protocol_version": 0
                    }})))
                    .await
                    .unwrap();
                }
                3 => {
                    ws_log.lock().await.heartbeats += 1;
                    let t = payload["d"]["t"].clone();
                    ws.send(send(json!({"op": 6, "d": {"t": t}}))).await.unwrap();
                }
                5 => ws_log
                    .lock()
                    .await
                    .speaking
                    .push(payload["d"]["speaking"].as_i64().unwrap()),
                _ => {}
            }
        }
        ws_log.lock().await.closed = true;
    });

    (endpoint, log, frames_rx)
}

pub async fn lavalink_ws(node: &TestNode) -> (mpsc::UnboundedReceiver<Value>, String) {
    let mut request = format!("ws://{}/v4/websocket", node.addr)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert("Authorization", PASSWORD.parse().unwrap());
    request.headers_mut().insert("User-Id", "111".parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut session_id = String::new();
    while let Some(Ok(msg)) = ws.next().await {
        if let Message::Text(text) = msg {
            let v: Value = serde_json::from_str(&text).unwrap();
            if v["op"] == "ready" {
                session_id = v["sessionId"].as_str().unwrap().to_string();
                break;
            }
        }
    }
    tokio::spawn(async move {
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(text) = msg {
                let _ = tx.send(serde_json::from_str(&text).unwrap());
            }
        }
    });
    (rx, session_id)
}

pub async fn wait_event(events: &mut mpsc::UnboundedReceiver<Value>, kind: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let v = tokio::time::timeout_at(deadline, events.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {kind}"))
            .unwrap();
        if v["op"] == "event" && v["type"] == kind {
            return v;
        }
    }
}

pub fn opus_samples(packet: &[u8]) -> usize {
    audiopus::packet::nb_samples(
        audiopus::packet::Packet::try_from(packet).unwrap(),
        audiopus::SampleRate::Hz48000,
    )
    .unwrap()
}
