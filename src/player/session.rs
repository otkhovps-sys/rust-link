//! Lavalink sessions: one per client websocket, resumable for `timeout` seconds.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use dashmap::DashMap;
use tokio::sync::mpsc;

use super::Player;
use crate::{
    api::models::{OutgoingMessage, SessionInfo, UpdateSession},
    node::NodeContext,
    plugin::PluginHost,
    util::random_session_id,
};

const DEFAULT_RESUME_TIMEOUT: u64 = 60;

struct Link {
    tx: Option<mpsc::UnboundedSender<String>>,
    backlog: VecDeque<String>,
    generation: u64,
}

pub struct Session {
    pub id: String,
    pub user_id: u64,
    pub client_name: String,
    players: DashMap<u64, Arc<Player>>,
    resuming: AtomicBool,
    timeout_secs: AtomicU64,
    link: Mutex<Link>,
    backlog_limit: usize,
    plugins: Option<Arc<PluginHost>>,
}

impl Session {
    pub fn players(&self) -> Vec<Arc<Player>> {
        self.players.iter().map(|p| Arc::clone(p.value())).collect()
    }

    pub fn player(&self, guild_id: u64) -> Option<Arc<Player>> {
        self.players.get(&guild_id).map(|p| Arc::clone(p.value()))
    }

    pub fn get_or_create_player(self: &Arc<Self>, guild_id: u64, ctx: &Arc<NodeContext>) -> Arc<Player> {
        Arc::clone(
            self.players
                .entry(guild_id)
                .or_insert_with(|| Player::new(guild_id, self, ctx))
                .value(),
        )
    }

    pub async fn destroy_player(&self, guild_id: u64) -> bool {
        match self.players.remove(&guild_id) {
            Some((_, player)) => {
                player.destroy().await;
                true
            }
            None => false,
        }
    }

    /// Serialize and deliver a message; buffered while a resumable session is detached.
    pub fn send(&self, message: &OutgoingMessage) {
        if let Some(plugins) = &self.plugins {
            plugins.dispatch(&self.id, message);
        }
        let Ok(json) = serde_json::to_string(message) else {
            return;
        };
        let mut link = self.link.lock().unwrap();
        if let Some(tx) = &link.tx {
            if let Err(mpsc::error::SendError(json)) = tx.send(json) {
                link.tx = None;
                self.buffer(&mut link, json);
            }
        } else {
            self.buffer(&mut link, json);
        }
    }

    fn buffer(&self, link: &mut Link, json: String) {
        if self.resuming.load(Ordering::Relaxed) {
            link.backlog.push_back(json);
            while link.backlog.len() > self.backlog_limit {
                link.backlog.pop_front();
            }
        }
    }

    /// Attach a websocket writer and flush events queued while detached.
    pub fn attach(&self, tx: mpsc::UnboundedSender<String>) {
        let mut link = self.link.lock().unwrap();
        for message in link.backlog.drain(..) {
            let _ = tx.send(message);
        }
        link.tx = Some(tx);
        link.generation += 1;
    }

    /// Returns the generation that a later resume would invalidate.
    pub fn detach(&self) -> u64 {
        let mut link = self.link.lock().unwrap();
        link.tx = None;
        link.generation
    }

    pub fn is_attached(&self) -> bool {
        self.link.lock().unwrap().tx.as_ref().is_some_and(|tx| !tx.is_closed())
    }

    pub fn generation(&self) -> u64 {
        self.link.lock().unwrap().generation
    }

    pub fn is_resuming(&self) -> bool {
        self.resuming.load(Ordering::Relaxed)
    }

    pub fn resume_timeout(&self) -> u64 {
        self.timeout_secs.load(Ordering::Relaxed)
    }

    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            resuming: self.is_resuming(),
            timeout: self.resume_timeout(),
        }
    }

    pub fn update(&self, update: UpdateSession) -> SessionInfo {
        if let Some(resuming) = update.resuming {
            self.resuming.store(resuming, Ordering::Relaxed);
        }
        if let Some(timeout) = update.timeout {
            self.timeout_secs.store(timeout, Ordering::Relaxed);
        }
        self.info()
    }

    pub async fn destroy(&self) {
        let guilds: Vec<u64> = self.players.iter().map(|p| *p.key()).collect();
        for guild in guilds {
            self.destroy_player(guild).await;
        }
    }
}

#[derive(Default)]
pub struct SessionManager {
    sessions: DashMap<String, Arc<Session>>,
    plugins: Option<Arc<PluginHost>>,
}

impl SessionManager {
    /// Sessions whose websocket messages are also forwarded to plugins.
    pub fn new(plugins: Arc<PluginHost>) -> Self {
        Self {
            sessions: DashMap::new(),
            plugins: (!plugins.is_empty()).then_some(plugins),
        }
    }

    pub fn plugins(&self) -> Option<&Arc<PluginHost>> {
        self.plugins.as_ref()
    }

    pub fn create(&self, user_id: u64, client_name: String, backlog_limit: usize) -> Arc<Session> {
        loop {
            let id = random_session_id();
            if self.sessions.contains_key(&id) {
                continue;
            }
            let session = Arc::new(Session {
                id: id.clone(),
                user_id,
                client_name,
                players: DashMap::new(),
                resuming: AtomicBool::new(false),
                timeout_secs: AtomicU64::new(DEFAULT_RESUME_TIMEOUT),
                link: Mutex::new(Link {
                    tx: None,
                    backlog: VecDeque::new(),
                    generation: 0,
                }),
                backlog_limit: backlog_limit.max(1),
                plugins: self.plugins.clone(),
            });
            self.sessions.insert(id, Arc::clone(&session));
            return session;
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.get(id).map(|s| Arc::clone(s.value()))
    }

    pub fn remove(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.remove(id).map(|(_, s)| s)
    }

    pub fn all(&self) -> Vec<Arc<Session>> {
        self.sessions.iter().map(|s| Arc::clone(s.value())).collect()
    }

    /// `(players, playing players)` across all sessions.
    pub fn player_counts(&self) -> (u32, u32) {
        let mut players = 0;
        let mut playing = 0;
        for session in self.sessions.iter() {
            for player in session.players.iter() {
                players += 1;
                playing += player.is_playing() as u32;
            }
        }
        (players, playing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::Stats;

    #[test]
    fn detached_resumable_session_buffers_events() {
        let manager = SessionManager::default();
        let session = manager.create(1, "test".into(), 2);
        let msg = OutgoingMessage::Stats(Stats::default());

        session.send(&msg); // not resuming: dropped
        session.update(UpdateSession {
            resuming: Some(true),
            timeout: Some(30),
        });
        for _ in 0..3 {
            session.send(&msg);
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        session.attach(tx);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err(), "backlog is capped at 2");
        assert!(session.is_attached());
        let generation = session.detach();
        assert_eq!(generation, 1);
        assert!(!session.is_attached());
    }
}
