//! Discord voice gateway (v8) client and the UDP media transport.
//!
//! Handshake: Hello(8) -> Identify(0) -> Ready(2) -> UDP IP discovery -> Select Protocol(1)
//! -> Session Description(4). Heartbeats (3/6) carry `seq_ack`; Resume(7)/Resumed(9) keep
//! the UDP session alive across gateway reconnects. DAVE opcodes are delegated to
//! [`DaveState`].

use std::{
    io,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering},
        Arc, Mutex, RwLock,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context};
use futures_util::{stream::SplitSink, SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{
    net::{TcpStream, UdpSocket},
    sync::mpsc,
    task::JoinHandle,
    time::{timeout, Instant, MissedTickBehavior},
};
use tokio_tungstenite::{
    tungstenite::{protocol::frame::coding::CloseCode, protocol::CloseFrame, Message},
    MaybeTlsStream, WebSocketStream,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::{
    crypto::{EncryptionMode, RtpPacketizer},
    dave::{opcodes as dave_op, DaveOutbound, DaveState},
};
use crate::util::{backoff, now_millis};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsStream, Message>;

mod op {
    pub const IDENTIFY: u8 = 0;
    pub const SELECT_PROTOCOL: u8 = 1;
    pub const READY: u8 = 2;
    pub const HEARTBEAT: u8 = 3;
    pub const SESSION_DESCRIPTION: u8 = 4;
    pub const SPEAKING: u8 = 5;
    pub const HEARTBEAT_ACK: u8 = 6;
    pub const RESUME: u8 = 7;
    pub const HELLO: u8 = 8;
    pub const RESUMED: u8 = 9;
}

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_FAILURES: u32 = 5;

#[derive(Debug, Clone)]
pub struct VoiceSettings {
    pub encryption_mode: String,
    pub dave: bool,
    pub udp_keepalive: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceConnectInfo {
    pub guild_id: u64,
    pub user_id: u64,
    pub channel_id: Option<u64>,
    pub session_id: String,
    pub token: String,
    pub endpoint: String,
}

#[derive(Debug, Clone)]
pub enum VoiceEvent {
    /// The Discord voice websocket closed (`WebSocketClosedEvent`).
    Closed { code: u16, reason: String, by_remote: bool },
}

/// Encrypted RTP sender bound to one voice session (SSRC + secret key).
pub struct VoiceTransport {
    socket: UdpSocket,
    ssrc: u32,
    packetizer: Mutex<RtpPacketizer>,
    dave: Arc<Mutex<DaveState>>,
    speaking_tx: mpsc::UnboundedSender<bool>,
    speaking: AtomicBool,
    keepalive_counter: AtomicU32,
}

impl VoiceTransport {
    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    pub fn encryption_mode(&self) -> EncryptionMode {
        self.packetizer.lock().unwrap().mode()
    }

    /// DAVE encrypt (when active), RTP packetize + transport encrypt, send over UDP.
    pub async fn send_opus(&self, frame: &[u8]) -> io::Result<()> {
        let packet = {
            let mut dave = self.dave.lock().unwrap();
            let payload = dave.encrypt_opus(frame);
            let mut packet = Vec::with_capacity(payload.len() + 40);
            self.packetizer
                .lock()
                .unwrap()
                .packetize(&payload, &mut packet)
                .map_err(io::Error::other)?;
            packet
        };
        self.socket.send(&packet).await.map(|_| ())
    }

    /// Discord requires a Speaking(5) update before audio is accepted.
    pub fn set_speaking(&self, speaking: bool) {
        if self.speaking.swap(speaking, Ordering::Relaxed) != speaking {
            let _ = self.speaking_tx.send(speaking);
        }
    }

    async fn send_keepalive(&self) {
        let counter = self.keepalive_counter.fetch_add(1, Ordering::Relaxed);
        let mut packet = [0u8; 8];
        packet[..4].copy_from_slice(&counter.to_le_bytes());
        let _ = self.socket.send(&packet).await;
    }
}

#[derive(Default)]
struct VoiceShared {
    transport: RwLock<Option<Arc<VoiceTransport>>>,
    ping_ms: AtomicI64,
    connected: AtomicBool,
}

/// Running gateway connection for one guild. Dropping it closes the connection.
pub struct VoiceConnection {
    info: VoiceConnectInfo,
    shared: Arc<VoiceShared>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl VoiceConnection {
    pub fn connect(info: VoiceConnectInfo, settings: VoiceSettings, events: mpsc::UnboundedSender<VoiceEvent>) -> Self {
        let shared = Arc::new(VoiceShared {
            ping_ms: AtomicI64::new(-1),
            ..Default::default()
        });
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(info.clone(), settings, Arc::clone(&shared), events, cancel.clone()));
        Self {
            info,
            shared,
            cancel,
            task,
        }
    }

    pub fn info(&self) -> &VoiceConnectInfo {
        &self.info
    }

    pub fn transport(&self) -> Option<Arc<VoiceTransport>> {
        self.shared.transport.read().unwrap().clone()
    }

    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Relaxed)
    }

    pub fn ping(&self) -> i64 {
        self.shared.ping_ms.load(Ordering::Relaxed)
    }

    /// The gateway task gave up (fatal close code or too many failures).
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl Drop for VoiceConnection {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Player-owned slot holding the current voice connection.
#[derive(Default)]
pub struct VoiceHandle {
    conn: Mutex<Option<VoiceConnection>>,
}

impl VoiceHandle {
    pub fn replace(&self, conn: Option<VoiceConnection>) {
        let old = std::mem::replace(&mut *self.conn.lock().unwrap(), conn);
        drop(old);
    }

    pub fn transport(&self) -> Option<Arc<VoiceTransport>> {
        self.conn.lock().unwrap().as_ref().and_then(|c| c.transport())
    }

    pub fn is_connected(&self) -> bool {
        self.conn.lock().unwrap().as_ref().is_some_and(|c| c.is_connected())
    }

    pub fn ping(&self) -> i64 {
        self.conn.lock().unwrap().as_ref().map_or(-1, |c| c.ping())
    }

    /// Same voice server credentials and the connection is still alive.
    pub fn matches(&self, info: &VoiceConnectInfo) -> bool {
        self.conn
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|c| c.info() == info && !c.is_finished())
    }
}

#[derive(Debug, Deserialize)]
struct GatewayPayload {
    op: u8,
    #[serde(default)]
    d: Value,
    #[serde(default)]
    seq: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Hello {
    heartbeat_interval: f64,
}

#[derive(Debug, Deserialize)]
struct Ready {
    ssrc: u32,
    ip: String,
    port: u16,
    modes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SessionDescription {
    mode: String,
    secret_key: Vec<u8>,
    #[serde(default)]
    dave_protocol_version: u16,
}

enum Incoming {
    Json(GatewayPayload),
    Binary { op: u8, payload: Vec<u8> },
}

enum SessionEnd {
    Cancelled,
    Closed { code: u16, reason: String },
    Failed(anyhow::Error),
}

enum CloseAction {
    Resume,
    Reconnect,
    Stop,
}

fn classify_close(code: u16) -> CloseAction {
    match code {
        // auth failed, server not found, unknown protocol/encryption, disconnected
        // (kicked / channel deleted), DAVE required, call terminated, bad requests
        4001..=4005 | 4011 | 4012 | 4014 | 4016 | 4017 | 4020 | 4022 => CloseAction::Stop,
        4006 | 4009 => CloseAction::Reconnect,
        _ => CloseAction::Resume,
    }
}

/// Preserve Discord's endpoint port when constructing the voice gateway URL.
pub fn gateway_url(endpoint: &str) -> String {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
        return format!("{endpoint}/?v=8");
    }
    let host = endpoint.trim_start_matches("https://").trim_start_matches("http://");
    format!("wss://{host}/?v=8")
}

struct GatewayContext<'a> {
    info: &'a VoiceConnectInfo,
    settings: &'a VoiceSettings,
    shared: &'a VoiceShared,
    cancel: &'a CancellationToken,
    speaking_tx: &'a mpsc::UnboundedSender<bool>,
}

async fn run(
    info: VoiceConnectInfo,
    settings: VoiceSettings,
    shared: Arc<VoiceShared>,
    events: mpsc::UnboundedSender<VoiceEvent>,
    cancel: CancellationToken,
) {
    let (speaking_tx, mut speaking_rx) = mpsc::unbounded_channel::<bool>();
    let ctx = GatewayContext {
        info: &info,
        settings: &settings,
        shared: &shared,
        cancel: &cancel,
        speaking_tx: &speaking_tx,
    };
    let mut seq_ack: i64 = -1;
    let mut resume: Option<Arc<VoiceTransport>> = None;
    let mut failures = 0u32;

    loop {
        let mut reached_ready = false;
        let end = session(&ctx, &mut speaking_rx, &mut seq_ack, resume.clone(), &mut reached_ready).await;
        shared.connected.store(false, Ordering::Relaxed);
        if reached_ready {
            failures = 0;
        }
        match end {
            SessionEnd::Cancelled => break,
            SessionEnd::Closed { code, reason } => {
                info!(guild = info.guild_id, code, %reason, "voice websocket closed by Discord");
                let _ = events.send(VoiceEvent::Closed {
                    code,
                    reason: reason.clone(),
                    by_remote: true,
                });
                match classify_close(code) {
                    CloseAction::Stop => break,
                    CloseAction::Reconnect => {
                        resume = None;
                        seq_ack = -1;
                        *shared.transport.write().unwrap() = None;
                    }
                    CloseAction::Resume => resume = shared.transport.read().unwrap().clone(),
                }
            }
            SessionEnd::Failed(error) => {
                warn!(guild = info.guild_id, error = %format!("{error:#}"), "voice gateway session failed");
                resume = shared.transport.read().unwrap().clone();
            }
        }
        failures += 1;
        if failures > MAX_FAILURES {
            warn!(guild = info.guild_id, "giving up on voice connection");
            let _ = events.send(VoiceEvent::Closed {
                code: 4900,
                reason: "voice connection failed repeatedly".into(),
                by_remote: false,
            });
            break;
        }
        let wait = backoff(failures, Duration::from_millis(500), Duration::from_secs(15));
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(wait) => {}
        }
    }
    *shared.transport.write().unwrap() = None;
    shared.connected.store(false, Ordering::Relaxed);
    debug!(guild = info.guild_id, "voice gateway task finished");
}

async fn send_json(sink: &mut WsSink, value: Value) -> anyhow::Result<()> {
    sink.send(Message::text(value.to_string())).await?;
    Ok(())
}

async fn send_dave(sink: &mut WsSink, outbound: Vec<DaveOutbound>) -> anyhow::Result<()> {
    for message in outbound {
        match message {
            DaveOutbound::Json(value) => send_json(sink, value).await?,
            DaveOutbound::Binary { op, payload } => {
                let mut frame = Vec::with_capacity(payload.len() + 1);
                frame.push(op);
                frame.extend_from_slice(&payload);
                sink.send(Message::binary(frame)).await?;
            }
        }
    }
    Ok(())
}

/// Read the next meaningful gateway message, tracking `seq_ack`.
async fn next_incoming(
    stream: &mut futures_util::stream::SplitStream<WsStream>,
    seq_ack: &mut i64,
) -> Result<Incoming, SessionEnd> {
    loop {
        match stream.next().await {
            None => return Err(SessionEnd::Failed(anyhow!("voice websocket ended without close frame"))),
            Some(Err(e)) => return Err(SessionEnd::Failed(e.into())),
            Some(Ok(Message::Text(text))) => match serde_json::from_str::<GatewayPayload>(&text) {
                Ok(payload) => {
                    if let Some(seq) = payload.seq {
                        *seq_ack = seq;
                    }
                    return Ok(Incoming::Json(payload));
                }
                Err(e) => debug!(%e, "ignoring malformed voice gateway payload"),
            },
            Some(Ok(Message::Binary(bytes))) if bytes.len() >= 3 => {
                *seq_ack = u16::from_be_bytes([bytes[0], bytes[1]]) as i64;
                return Ok(Incoming::Binary {
                    op: bytes[2],
                    payload: bytes[3..].to_vec(),
                });
            }
            Some(Ok(Message::Close(frame))) => {
                let (code, reason) = frame
                    .map(|f| (u16::from(f.code), f.reason.to_string()))
                    .unwrap_or((1005, String::new()));
                return Err(SessionEnd::Closed { code, reason });
            }
            Some(Ok(_)) => {}
        }
    }
}

/// Wait for a given opcode during the handshake; other messages are queued.
async fn expect_op(
    stream: &mut futures_util::stream::SplitStream<WsStream>,
    seq_ack: &mut i64,
    wanted: u8,
    queued: &mut Vec<Incoming>,
) -> Result<GatewayPayload, SessionEnd> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        let incoming = match tokio::time::timeout_at(deadline, next_incoming(stream, seq_ack)).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(SessionEnd::Failed(anyhow!(
                    "timed out waiting for voice opcode {wanted}"
                )))
            }
        };
        match incoming {
            Incoming::Json(payload) if payload.op == wanted => return Ok(payload),
            other => queued.push(other),
        }
    }
}

fn parse<T: serde::de::DeserializeOwned>(payload: GatewayPayload) -> Result<T, SessionEnd> {
    serde_json::from_value(payload.d)
        .map_err(|e| SessionEnd::Failed(anyhow!("invalid payload for opcode {}: {e}", payload.op)))
}

async fn session(
    ctx: &GatewayContext<'_>,
    speaking_rx: &mut mpsc::UnboundedReceiver<bool>,
    seq_ack: &mut i64,
    resume: Option<Arc<VoiceTransport>>,
    reached_ready: &mut bool,
) -> SessionEnd {
    let info = ctx.info;
    let url = gateway_url(&info.endpoint);
    let ws = tokio::select! {
        _ = ctx.cancel.cancelled() => return SessionEnd::Cancelled,
        result = timeout(HANDSHAKE_TIMEOUT, tokio_tungstenite::connect_async(url.as_str())) => match result {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(e)) => return SessionEnd::Failed(anyhow::Error::new(e).context(format!("cannot connect to {url}"))),
            Err(_) => return SessionEnd::Failed(anyhow!("timed out connecting to {url}")),
        },
    };
    let (mut sink, mut stream) = ws.split();
    let mut queued = Vec::new();

    let handshake = async {
        let hello: Hello = parse(expect_op(&mut stream, seq_ack, op::HELLO, &mut queued).await?)?;
        let fail = |e: anyhow::Error| SessionEnd::Failed(e);

        let transport = if let Some(transport) = resume {
            send_json(
                &mut sink,
                json!({"op": op::RESUME, "d": {
                    "server_id": info.guild_id.to_string(),
                    "session_id": info.session_id,
                    "token": info.token,
                    "seq_ack": *seq_ack,
                }}),
            )
            .await
            .map_err(fail)?;
            expect_op(&mut stream, seq_ack, op::RESUMED, &mut queued).await?;
            info!(guild = info.guild_id, "voice session resumed");
            transport
        } else {
            let dave_version = match (ctx.settings.dave, info.channel_id) {
                (true, Some(_)) => DaveState::MAX_PROTOCOL_VERSION,
                _ => 0,
            };
            send_json(
                &mut sink,
                json!({"op": op::IDENTIFY, "d": {
                    "server_id": info.guild_id.to_string(),
                    "user_id": info.user_id.to_string(),
                    "session_id": info.session_id,
                    "token": info.token,
                    "max_dave_protocol_version": dave_version,
                }}),
            )
            .await
            .map_err(fail)?;
            let ready: Ready = parse(expect_op(&mut stream, seq_ack, op::READY, &mut queued).await?)?;
            let mode = EncryptionMode::negotiate(&ready.modes, &ctx.settings.encryption_mode)
                .ok_or_else(|| SessionEnd::Failed(anyhow!("no supported encryption mode in {:?}", ready.modes)))?;

            let socket = connect_udp(&ready.ip, ready.port).await.map_err(fail)?;
            let (external_ip, external_port) = discover_ip(&socket, ready.ssrc).await.map_err(fail)?;
            debug!(guild = info.guild_id, %external_ip, external_port, mode = mode.as_str(), "udp discovery done");
            send_json(
                &mut sink,
                json!({"op": op::SELECT_PROTOCOL, "d": {
                    "protocol": "udp",
                    "data": {"address": external_ip, "port": external_port, "mode": mode.as_str()},
                }}),
            )
            .await
            .map_err(fail)?;

            let description: SessionDescription =
                parse(expect_op(&mut stream, seq_ack, op::SESSION_DESCRIPTION, &mut queued).await?)?;
            let mode = EncryptionMode::parse(&description.mode).unwrap_or(mode);
            let packetizer = RtpPacketizer::new(mode, &description.secret_key, ready.ssrc).map_err(fail)?;
            let mut dave = DaveState::new(info.user_id, info.channel_id.unwrap_or_default());
            let outbound = dave.on_session_description(description.dave_protocol_version);
            send_dave(&mut sink, outbound).await.map_err(fail)?;
            info!(
                guild = info.guild_id,
                ssrc = ready.ssrc,
                mode = mode.as_str(),
                dave = description.dave_protocol_version,
                "voice connection ready"
            );
            Arc::new(VoiceTransport {
                socket,
                ssrc: ready.ssrc,
                packetizer: Mutex::new(packetizer),
                dave: Arc::new(Mutex::new(dave)),
                speaking_tx: ctx.speaking_tx.clone(),
                speaking: AtomicBool::new(false),
                keepalive_counter: AtomicU32::new(0),
            })
        };
        Ok::<_, SessionEnd>((transport, hello))
    };

    let (transport, hello) = tokio::select! {
        _ = ctx.cancel.cancelled() => return SessionEnd::Cancelled,
        result = handshake => match result {
            Ok(v) => v,
            Err(end) => return end,
        },
    };

    *ctx.shared.transport.write().unwrap() = Some(Arc::clone(&transport));
    ctx.shared.connected.store(true, Ordering::Relaxed);
    *reached_ready = true;
    // Speaking state is re-announced on the new gateway connection.
    if transport.speaking.load(Ordering::Relaxed) {
        let _ = send_speaking(&mut sink, &transport, true).await;
    }

    for incoming in queued {
        if let Err(e) = handle_incoming(&mut sink, &transport, ctx.shared, incoming).await {
            return SessionEnd::Failed(e);
        }
    }

    let mut heartbeat = tokio::time::interval(Duration::from_millis(hello.heartbeat_interval.max(1000.0) as u64));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    heartbeat.tick().await;
    let mut keepalive = tokio::time::interval(ctx.settings.udp_keepalive.max(Duration::from_secs(1)));
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut unacked = 0u32;

    loop {
        tokio::select! {
            _ = ctx.cancel.cancelled() => {
                let _ = sink.send(Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() }))).await;
                return SessionEnd::Cancelled;
            }
            _ = heartbeat.tick() => {
                if unacked >= 2 {
                    return SessionEnd::Failed(anyhow!("voice heartbeat not acknowledged (zombie connection)"));
                }
                unacked += 1;
                if let Err(e) = send_json(&mut sink, json!({"op": op::HEARTBEAT, "d": {"t": now_millis(), "seq_ack": *seq_ack}})).await {
                    return SessionEnd::Failed(e);
                }
            }
            _ = keepalive.tick() => transport.send_keepalive().await,
            Some(speaking) = speaking_rx.recv() => {
                if let Err(e) = send_speaking(&mut sink, &transport, speaking).await {
                    return SessionEnd::Failed(e);
                }
            }
            incoming = next_incoming(&mut stream, seq_ack) => {
                let incoming = match incoming {
                    Ok(incoming) => incoming,
                    Err(end) => return end,
                };
                if matches!(&incoming, Incoming::Json(p) if p.op == op::HEARTBEAT_ACK) {
                    unacked = 0;
                }
                if let Err(e) = handle_incoming(&mut sink, &transport, ctx.shared, incoming).await {
                    return SessionEnd::Failed(e);
                }
            }
        }
    }
}

async fn send_speaking(sink: &mut WsSink, transport: &VoiceTransport, speaking: bool) -> anyhow::Result<()> {
    send_json(
        sink,
        json!({"op": op::SPEAKING, "d": {"speaking": if speaking { 1 } else { 0 }, "delay": 0, "ssrc": transport.ssrc}}),
    )
    .await
}

async fn handle_incoming(
    sink: &mut WsSink,
    transport: &VoiceTransport,
    shared: &VoiceShared,
    incoming: Incoming,
) -> anyhow::Result<()> {
    let outbound = match incoming {
        Incoming::Json(payload) => match payload.op {
            op::HEARTBEAT_ACK => {
                if let Some(sent) = payload.d.get("t").and_then(Value::as_i64) {
                    shared.ping_ms.store((now_millis() - sent).max(0), Ordering::Relaxed);
                }
                Vec::new()
            }
            dave_op::CLIENTS_CONNECT
            | dave_op::CLIENT_DISCONNECT
            | dave_op::PREPARE_TRANSITION
            | dave_op::EXECUTE_TRANSITION
            | dave_op::PREPARE_EPOCH => transport.dave.lock().unwrap().on_json(payload.op, &payload.d),
            _ => Vec::new(),
        },
        Incoming::Binary { op, payload } => transport.dave.lock().unwrap().on_binary(op, &payload),
    };
    send_dave(sink, outbound).await
}

async fn connect_udp(ip: &str, port: u16) -> anyhow::Result<UdpSocket> {
    let remote: SocketAddr = tokio::net::lookup_host((ip, port))
        .await?
        .next()
        .ok_or_else(|| anyhow!("cannot resolve voice server {ip}:{port}"))?;
    let bind: SocketAddr = if remote.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let socket = UdpSocket::bind(bind).await?;
    socket.connect(remote).await.context("udp connect")?;
    Ok(socket)
}

/// Discord IP discovery: 74-byte request (type 1), response (type 2) carries our public
/// address and port as seen by the voice server.
pub async fn discover_ip(socket: &UdpSocket, ssrc: u32) -> anyhow::Result<(String, u16)> {
    let mut request = [0u8; 74];
    request[0..2].copy_from_slice(&1u16.to_be_bytes());
    request[2..4].copy_from_slice(&70u16.to_be_bytes());
    request[4..8].copy_from_slice(&ssrc.to_be_bytes());
    let mut response = [0u8; 128];
    for _ in 0..5 {
        socket.send(&request).await?;
        if let Ok(Ok(n)) = timeout(Duration::from_secs(1), socket.recv(&mut response)).await {
            if n >= 74 && u16::from_be_bytes([response[0], response[1]]) == 2 {
                let address = &response[8..72];
                let end = address.iter().position(|b| *b == 0).unwrap_or(address.len());
                let ip = String::from_utf8_lossy(&address[..end]).to_string();
                let port = u16::from_be_bytes([response[72], response[73]]);
                return Ok((ip, port));
            }
        }
    }
    bail!("udp ip discovery timed out")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_gateway_urls() {
        assert_eq!(
            gateway_url("c-sin05.discord.media:443"),
            "wss://c-sin05.discord.media:443/?v=8"
        );
        assert_eq!(
            gateway_url("c-sin08-993a75b5.discord.media:2096"),
            "wss://c-sin08-993a75b5.discord.media:2096/?v=8"
        );
        assert_eq!(
            gateway_url(" https://c-sin05.discord.media:2053/ "),
            "wss://c-sin05.discord.media:2053/?v=8"
        );
        assert_eq!(gateway_url("c-sin05.discord.media"), "wss://c-sin05.discord.media/?v=8");
        assert_eq!(gateway_url("ws://127.0.0.1:9000"), "ws://127.0.0.1:9000/?v=8");
        assert_eq!(
            gateway_url("wss://c-sin05.discord.media:2096/"),
            "wss://c-sin05.discord.media:2096/?v=8"
        );
        assert_eq!(gateway_url("[::1]:2096"), "wss://[::1]:2096/?v=8");
    }

    #[test]
    fn close_codes() {
        assert!(matches!(classify_close(4014), CloseAction::Stop));
        assert!(matches!(classify_close(4006), CloseAction::Reconnect));
        assert!(matches!(classify_close(4015), CloseAction::Resume));
        assert!(matches!(classify_close(1006), CloseAction::Resume));
    }
}
