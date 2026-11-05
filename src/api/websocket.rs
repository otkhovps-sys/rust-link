//! `GET /v4/websocket`: Lavalink event channel (ready, playerUpdate, stats, event).

use std::{sync::Arc, time::Duration};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, info};

use super::{models::OutgoingMessage, ApiError, AppState};
use crate::player::Session;

pub async fn websocket_handler(State(ctx): State<AppState>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let Some(user_id) = header("User-Id").and_then(|v| v.parse::<u64>().ok()) else {
        return ApiError::bad_request("Missing or invalid User-Id header").into_response();
    };
    let client_name = header("Client-Name").unwrap_or_else(|| "unknown".into());

    let resumed = header("Session-Id")
        .and_then(|id| ctx.sessions.get(&id))
        .filter(|s| s.is_resuming() && !s.is_attached() && s.user_id == user_id);
    let (session, resumed) = match resumed {
        Some(session) => (session, true),
        None => (
            ctx.sessions
                .create(user_id, client_name.clone(), ctx.config.lavalink.resume_backlog_size),
            false,
        ),
    };
    info!(session = %session.id, %client_name, user_id, resumed, "client connected");

    let socket_ctx = Arc::clone(&ctx);
    let socket_session = Arc::clone(&session);
    let mut response = ws.on_upgrade(move |socket| handle_socket(socket, socket_ctx, socket_session, resumed));
    response.headers_mut().insert(
        "Session-Resumed",
        HeaderValue::from_static(if resumed { "true" } else { "false" }),
    );
    response
}

async fn handle_socket(socket: WebSocket, ctx: AppState, session: Arc<Session>, resumed: bool) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    let ready = OutgoingMessage::Ready {
        resumed,
        session_id: session.id.clone(),
    };
    let _ = tx.send(serde_json::to_string(&ready).unwrap_or_default());
    session.attach(tx.clone());
    // Initial stats so load balancing clients get numbers right away.
    let _ = tx.send(serde_json::to_string(&OutgoingMessage::Stats(ctx.stats(None))).unwrap_or_default());
    drop(tx);

    loop {
        tokio::select! {
            _ = ctx.shutdown.cancelled() => {
                let _ = sink.send(Message::Close(None)).await;
                break;
            }
            outgoing = rx.recv() => match outgoing {
                Some(text) => {
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            incoming = stream.next() => match incoming {
                // Lavalink v4 clients do not send ops over the websocket; pings are answered
                // by axum automatically.
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
        }
    }

    let generation = session.detach();
    if ctx.shutdown.is_cancelled() {
        return;
    }
    if session.is_resuming() {
        let timeout = session.resume_timeout();
        info!(session = %session.id, timeout, "client disconnected, session kept for resuming");
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(timeout)).await;
            if session.generation() == generation && !session.is_attached() {
                info!(session = %session.id, "resume timeout elapsed, destroying session");
                ctx.sessions.remove(&session.id);
                session.destroy().await;
            }
        });
    } else {
        debug!(session = %session.id, "client disconnected, destroying session");
        ctx.sessions.remove(&session.id);
        session.destroy().await;
    }
}
