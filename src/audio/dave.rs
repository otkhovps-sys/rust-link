//! Discord DAVE (audio end-to-end encryption) state machine for the voice gateway.
//!
//! Opcodes handled (https://daveprotocol.com):
//! - 11 / 13 clients connect / disconnect (recognized user IDs)
//! - 21 prepare transition, 22 execute transition, 23 transition ready (sent),
//!   24 prepare epoch, 31 invalid commit/welcome (sent)
//! - binary: 25 external sender, 26 key package (sent), 27 proposals,
//!   28 commit/welcome (sent), 29 announce commit, 30 welcome

use std::borrow::Cow;

#[cfg(feature = "dave")]
use serde_json::json;
use serde_json::Value;

/// A message the gateway must send back to Discord.
#[derive(Debug, Clone, PartialEq)]
pub enum DaveOutbound {
    Json(Value),
    Binary { op: u8, payload: Vec<u8> },
}

pub mod opcodes {
    pub const CLIENTS_CONNECT: u8 = 11;
    pub const CLIENT_DISCONNECT: u8 = 13;
    pub const PREPARE_TRANSITION: u8 = 21;
    pub const EXECUTE_TRANSITION: u8 = 22;
    pub const TRANSITION_READY: u8 = 23;
    pub const PREPARE_EPOCH: u8 = 24;
    pub const MLS_EXTERNAL_SENDER: u8 = 25;
    pub const MLS_KEY_PACKAGE: u8 = 26;
    pub const MLS_PROPOSALS: u8 = 27;
    pub const MLS_COMMIT_WELCOME: u8 = 28;
    pub const MLS_ANNOUNCE_COMMIT: u8 = 29;
    pub const MLS_WELCOME: u8 = 30;
    pub const MLS_INVALID_COMMIT_WELCOME: u8 = 31;
}

#[cfg(feature = "dave")]
fn transition_ready(transition_id: u16) -> DaveOutbound {
    DaveOutbound::Json(json!({"op": opcodes::TRANSITION_READY, "d": {"transition_id": transition_id}}))
}

#[cfg(feature = "dave")]
mod imp {
    use std::{
        collections::{HashMap, HashSet},
        num::NonZeroU16,
    };

    use davey::{DaveSession, ProposalsOperationType};
    use tracing::{debug, warn};

    use super::*;

    pub struct DaveState {
        session: Option<DaveSession>,
        protocol_version: u16,
        user_id: u64,
        channel_id: u64,
        connected_users: HashSet<u64>,
        pending_transitions: HashMap<u16, u16>,
    }

    impl DaveState {
        pub const MAX_PROTOCOL_VERSION: u16 = davey::DAVE_PROTOCOL_VERSION;

        pub fn new(user_id: u64, channel_id: u64) -> Self {
            Self {
                session: None,
                protocol_version: 0,
                user_id,
                channel_id,
                connected_users: HashSet::new(),
                pending_transitions: HashMap::new(),
            }
        }

        pub fn protocol_version(&self) -> u16 {
            self.protocol_version
        }

        fn recognized_users(&self) -> Vec<u64> {
            let mut users: Vec<u64> = self.connected_users.iter().copied().collect();
            users.push(self.user_id);
            users
        }

        /// (Re)create the MLS session for the current protocol version and announce a
        /// fresh key package.
        fn reinit(&mut self) -> Vec<DaveOutbound> {
            let Some(version) = NonZeroU16::new(self.protocol_version) else {
                if let Some(session) = &mut self.session {
                    let _ = session.reset();
                    session.set_passthrough_mode(true, Some(10));
                }
                return Vec::new();
            };
            let result = if let Some(session) = self.session.as_mut() {
                session
                    .reinit(version, self.user_id, self.channel_id, None)
                    .map_err(|e| e.to_string())
            } else {
                DaveSession::new(version, self.user_id, self.channel_id, None)
                    .map(|session| self.session = Some(session))
                    .map_err(|e| e.to_string())
            };
            if let Err(e) = result {
                warn!(error = %e, "DAVE session init failed");
                return Vec::new();
            }
            match self.session.as_mut().map(|s| s.create_key_package()) {
                Some(Ok(key_package)) => vec![DaveOutbound::Binary {
                    op: opcodes::MLS_KEY_PACKAGE,
                    payload: key_package,
                }],
                Some(Err(e)) => {
                    warn!(error = %e, "DAVE key package creation failed");
                    Vec::new()
                }
                None => Vec::new(),
            }
        }

        fn recover(&mut self, transition_id: u16) -> Vec<DaveOutbound> {
            let mut out = vec![DaveOutbound::Json(json!({
                "op": opcodes::MLS_INVALID_COMMIT_WELCOME,
                "d": {"transition_id": transition_id}
            }))];
            out.extend(self.reinit());
            out
        }

        fn prepare(&mut self, transition_id: u16, protocol_version: u16) -> Vec<DaveOutbound> {
            if let Some(session) = &mut self.session {
                if let Err(e) = session.prepare_transition(transition_id, protocol_version) {
                    warn!(error = %e, transition_id, "DAVE prepare transition failed");
                    return self.recover(transition_id);
                }
            }
            if transition_id == 0 {
                self.protocol_version = protocol_version;
                Vec::new()
            } else {
                self.pending_transitions.insert(transition_id, protocol_version);
                vec![transition_ready(transition_id)]
            }
        }

        pub fn on_session_description(&mut self, protocol_version: u16) -> Vec<DaveOutbound> {
            self.protocol_version = protocol_version;
            self.reinit()
        }

        pub fn on_json(&mut self, op: u8, d: &Value) -> Vec<DaveOutbound> {
            let field = |name: &str| d.get(name).and_then(Value::as_u64).unwrap_or(0) as u16;
            match op {
                opcodes::CLIENTS_CONNECT => {
                    let ids = d.get("user_ids").and_then(Value::as_array).cloned().unwrap_or_default();
                    for id in ids {
                        if let Some(id) = id.as_str().and_then(|s| s.parse().ok()) {
                            self.connected_users.insert(id);
                        }
                    }
                    Vec::new()
                }
                opcodes::CLIENT_DISCONNECT => {
                    if let Some(id) = d
                        .get("user_id")
                        .and_then(Value::as_str)
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        self.connected_users.remove(&id);
                    }
                    Vec::new()
                }
                opcodes::PREPARE_TRANSITION => self.prepare(field("transition_id"), field("protocol_version")),
                opcodes::EXECUTE_TRANSITION => {
                    let transition_id = field("transition_id");
                    if let Some(session) = &mut self.session {
                        if let Err(e) = session.execute_transition(transition_id) {
                            warn!(error = %e, transition_id, "DAVE execute transition failed");
                        }
                    }
                    if let Some(version) = self.pending_transitions.remove(&transition_id) {
                        self.protocol_version = version;
                    }
                    debug!(
                        transition_id,
                        version = self.protocol_version,
                        "DAVE transition executed"
                    );
                    Vec::new()
                }
                opcodes::PREPARE_EPOCH => {
                    if field("epoch") == 1 {
                        self.protocol_version = field("protocol_version");
                        return self.reinit();
                    }
                    Vec::new()
                }
                _ => Vec::new(),
            }
        }

        pub fn on_binary(&mut self, op: u8, payload: &[u8]) -> Vec<DaveOutbound> {
            if self.session.is_none() && self.protocol_version > 0 {
                let _ = self.reinit();
            }
            let recognized = self.recognized_users();
            let Some(session) = self.session.as_mut() else {
                return Vec::new();
            };
            match op {
                opcodes::MLS_EXTERNAL_SENDER => {
                    if let Err(e) = session.set_external_sender(payload) {
                        warn!(error = %e, "DAVE external sender rejected");
                    }
                    Vec::new()
                }
                opcodes::MLS_PROPOSALS if !payload.is_empty() => {
                    let operation = if payload[0] == 0 {
                        ProposalsOperationType::APPEND
                    } else {
                        ProposalsOperationType::REVOKE
                    };
                    match session.process_proposals(operation, &payload[1..], Some(&recognized)) {
                        Ok(Some(commit_welcome)) => {
                            let mut out = commit_welcome.commit;
                            if let Some(welcome) = commit_welcome.welcome {
                                out.extend_from_slice(&welcome);
                            }
                            vec![DaveOutbound::Binary {
                                op: opcodes::MLS_COMMIT_WELCOME,
                                payload: out,
                            }]
                        }
                        Ok(None) => Vec::new(),
                        Err(e) => {
                            warn!(error = %e, "DAVE proposals rejected");
                            Vec::new()
                        }
                    }
                }
                opcodes::MLS_ANNOUNCE_COMMIT | opcodes::MLS_WELCOME if payload.len() >= 2 => {
                    let transition_id = u16::from_be_bytes([payload[0], payload[1]]);
                    let body = &payload[2..];
                    let result = if op == opcodes::MLS_ANNOUNCE_COMMIT {
                        session.process_commit(body).map_err(|e| e.to_string())
                    } else {
                        session
                            .process_welcome(body, Some(&recognized))
                            .map(|_| true)
                            .map_err(|e| e.to_string())
                    };
                    match result {
                        Ok(true) => {
                            let version = self.protocol_version;
                            self.prepare(transition_id, version)
                        }
                        Ok(false) => Vec::new(),
                        Err(e) => {
                            warn!(error = %e, transition_id, op, "DAVE commit/welcome rejected");
                            self.recover(transition_id)
                        }
                    }
                }
                _ => Vec::new(),
            }
        }

        /// End-to-end encrypt an Opus frame when the MLS group is ready.
        pub fn encrypt_opus<'a>(&mut self, frame: &'a [u8]) -> Cow<'a, [u8]> {
            if self.protocol_version == 0 {
                return Cow::Borrowed(frame);
            }
            match self.session.as_mut().map(|s| s.encrypt_opus(frame)) {
                Some(Ok(encrypted)) => encrypted,
                _ => Cow::Borrowed(frame),
            }
        }
    }
}

#[cfg(not(feature = "dave"))]
mod imp {
    use super::*;

    /// DAVE disabled at compile time: the node identifies with protocol version 0.
    pub struct DaveState;

    impl DaveState {
        pub const MAX_PROTOCOL_VERSION: u16 = 0;

        pub fn new(_user_id: u64, _channel_id: u64) -> Self {
            Self
        }

        pub fn protocol_version(&self) -> u16 {
            0
        }

        pub fn on_session_description(&mut self, _protocol_version: u16) -> Vec<DaveOutbound> {
            Vec::new()
        }

        pub fn on_json(&mut self, _op: u8, _d: &Value) -> Vec<DaveOutbound> {
            Vec::new()
        }

        pub fn on_binary(&mut self, _op: u8, _payload: &[u8]) -> Vec<DaveOutbound> {
            Vec::new()
        }

        pub fn encrypt_opus<'a>(&mut self, frame: &'a [u8]) -> Cow<'a, [u8]> {
            Cow::Borrowed(frame)
        }
    }
}

pub use imp::DaveState;

#[cfg(all(test, feature = "dave"))]
mod tests {
    use super::*;

    #[test]
    fn session_description_emits_key_package() {
        let mut state = DaveState::new(1234, 5678);
        let out = state.on_session_description(1);
        assert_eq!(state.protocol_version(), 1);
        assert!(matches!(&out[..], [DaveOutbound::Binary { op: 26, payload }] if !payload.is_empty()));
    }

    #[test]
    fn transitions_answer_with_ready() {
        let mut state = DaveState::new(1, 2);
        state.on_session_description(1);
        let out = state.on_json(
            opcodes::PREPARE_TRANSITION,
            &json!({"transition_id": 7, "protocol_version": 0}),
        );
        assert_eq!(out, vec![transition_ready(7)]);
        assert_eq!(state.protocol_version(), 1);
        state.on_json(opcodes::EXECUTE_TRANSITION, &json!({"transition_id": 7}));
        assert_eq!(state.protocol_version(), 0);
        // Downgraded: frames go out untouched.
        assert_eq!(&*state.encrypt_opus(&[1, 2, 3]), &[1, 2, 3]);
    }

    #[test]
    fn invalid_commit_requests_recovery() {
        let mut state = DaveState::new(1, 2);
        state.on_session_description(1);
        let out = state.on_binary(opcodes::MLS_ANNOUNCE_COMMIT, &[0, 9, 0xde, 0xad]);
        // Without an established group the commit is ignored rather than rejected.
        assert!(out.is_empty() || matches!(&out[0], DaveOutbound::Json(v) if v["op"] == 31));
    }
}
