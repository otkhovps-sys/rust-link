//! Plugins: separate programs, written in any language, that extend the node.
//!
//! A plugin is started as a child process and speaks JSON-RPC 2.0, one JSON object per
//! line, over stdin/stdout (stderr goes to the node log). After `initialize` it tells the
//! node what it handles:
//!
//! - search prefixes (`radio:lofi`) and URL patterns, answered with `loadItem`;
//! - playback of its own tracks (`resolve` to a URL, a local file or another track);
//! - websocket events (`TrackStartEvent`, `playerUpdate`, ...), sent as `event`;
//! - extra REST routes (`GET /v4/lyrics/{guildId}`), answered with `route`.
//!
//! Plugins can call the node's own REST API back with `rest`, and log with `log`.

pub mod rpc;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, Weak},
    time::Duration,
};

use anyhow::{bail, Context};
use axum::body::Body;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use tracing::{debug, info, warn};

use self::rpc::{InboundHandler, LaunchSpec, INVALID_PARAMS, METHOD_NOT_FOUND};
pub use self::rpc::{PluginError, PluginProcess, PluginState};
use crate::{
    api::models::{
        self, LoadResult, OutgoingMessage, PlaylistData, PlaylistInfo, Severity, Track, TrackInfo, STREAM_LENGTH,
    },
    config::{AppConfig, PluginEntry, PluginsConfig, PLUGIN_HOST_KEYS},
    node::NodeContext,
};

/// Version of the stdin/stdout protocol, sent in `initialize`.
pub const PROTOCOL_VERSION: u32 = 1;
const MANIFEST_FILES: [&str; 3] = ["plugin.yml", "plugin.yaml", "plugin.json"];
/// Largest REST response body handed back to a plugin.
const REST_BODY_LIMIT: usize = 64 * 1024 * 1024;
/// Messages sent every few seconds; a plugin only gets them when it names them.
const PERIODIC_OPS: [&str; 2] = ["playerUpdate", "stats"];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Param(String),
    Rest(String),
}

/// A REST route registered by a plugin, e.g. `GET /v4/lyrics/{guildId}`.
#[derive(Debug, Clone)]
pub struct RouteSpec {
    /// Upper case, or `*` for any method.
    pub method: String,
    pub path: String,
    segments: Vec<Segment>,
}

impl RouteSpec {
    fn parse(method: &str, path: &str) -> Result<Self, String> {
        if !path.starts_with('/') {
            return Err(format!("route path {path:?} must start with /"));
        }
        let mut segments = Vec::new();
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        for (i, part) in parts.iter().enumerate() {
            let segment = match part.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
                Some(name) if name.starts_with('*') => {
                    if i + 1 != parts.len() {
                        return Err(format!("{path}: {{*{}}} must be the last segment", &name[1..]));
                    }
                    Segment::Rest(name[1..].to_string())
                }
                Some(name) => Segment::Param(name.to_string()),
                None => Segment::Literal(part.to_string()),
            };
            segments.push(segment);
        }
        Ok(Self {
            method: method.trim().to_ascii_uppercase(),
            path: path.to_string(),
            segments,
        })
    }

    /// Path parameters when `method path` matches.
    fn matches(&self, method: &str, path: &str) -> Option<Map<String, Value>> {
        if self.method != "*" && !self.method.eq_ignore_ascii_case(method) {
            return None;
        }
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut params = Map::new();
        for (i, segment) in self.segments.iter().enumerate() {
            match segment {
                Segment::Literal(literal) => {
                    if parts.get(i) != Some(&literal.as_str()) {
                        return None;
                    }
                }
                Segment::Param(name) => {
                    let value = parts.get(i)?;
                    params.insert(name.clone(), Value::String(decode_segment(value)));
                }
                Segment::Rest(name) => {
                    let rest = parts.get(i..).unwrap_or_default().join("/");
                    params.insert(name.clone(), Value::String(decode_segment(&rest)));
                    return Some(params);
                }
            }
        }
        (parts.len() == self.segments.len()).then_some(params)
    }
}

fn decode_segment(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// What a plugin handles, from its answer to `initialize`.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    /// `sourceName` of the tracks it creates (the first one is the default).
    pub source_names: Vec<String>,
    /// Lower case, without the trailing colon.
    pub search_prefixes: Vec<String>,
    pub url_patterns: Vec<regex::Regex>,
    pub events: Vec<String>,
    pub routes: Vec<RouteSpec>,
    /// Implements `resolve` for its tracks.
    pub resolve: bool,
    /// Its tracks may be kept in the audio cache.
    pub cache: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRoute {
    #[serde(default = "any_method")]
    method: String,
    path: String,
}

fn any_method() -> String {
    "GET".into()
}

#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawManifest {
    name: Option<String>,
    version: Option<String>,
    source_name: Option<String>,
    source_names: Vec<String>,
    search_prefixes: Vec<String>,
    url_patterns: Vec<String>,
    events: Vec<String>,
    routes: Vec<RawRoute>,
    resolve: bool,
    cache: bool,
}

impl Default for RawManifest {
    fn default() -> Self {
        Self {
            name: None,
            version: None,
            source_name: None,
            source_names: Vec::new(),
            search_prefixes: Vec::new(),
            url_patterns: Vec::new(),
            events: Vec::new(),
            routes: Vec::new(),
            resolve: false,
            cache: true,
        }
    }
}

impl Manifest {
    pub fn parse(value: Value, plugin: &str) -> Result<Self, String> {
        let raw: RawManifest = if value.is_null() {
            RawManifest::default()
        } else {
            serde_json::from_value(value).map_err(|e| format!("invalid manifest: {e}"))?
        };
        let mut source_names: Vec<String> = raw.source_name.into_iter().chain(raw.source_names).collect();
        source_names.dedup();
        let url_patterns = raw
            .url_patterns
            .iter()
            .map(|p| regex::Regex::new(p).map_err(|e| format!("invalid urlPattern {p:?}: {e}")))
            .collect::<Result<_, _>>()?;
        let routes = raw
            .routes
            .iter()
            .map(|r| RouteSpec::parse(&r.method, &r.path))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            name: raw.name.unwrap_or_else(|| plugin.to_string()),
            version: raw.version.unwrap_or_else(|| "0.0.0".into()),
            source_names,
            search_prefixes: raw
                .search_prefixes
                .iter()
                .map(|p| p.trim().trim_end_matches(':').to_ascii_lowercase())
                .filter(|p| !p.is_empty())
                .collect(),
            url_patterns,
            events: raw.events,
            routes,
            resolve: raw.resolve,
            cache: raw.cache,
        })
    }

    fn default_source(&self, plugin: &str) -> String {
        self.source_names.first().cloned().unwrap_or_else(|| plugin.to_string())
    }

    /// `loadItem` parameters when the identifier belongs to this plugin.
    fn claim(&self, identifier: &str) -> Option<Value> {
        if let Some((prefix, query)) = identifier.split_once(':') {
            let prefix = prefix.to_ascii_lowercase();
            if self.search_prefixes.contains(&prefix) {
                return Some(json!({"identifier": identifier, "prefix": prefix, "query": query.trim()}));
            }
        }
        self.url_patterns
            .iter()
            .any(|p| p.is_match(identifier))
            .then(|| json!({"identifier": identifier, "url": identifier}))
    }

    fn subscribes(&self, op: &str, kind: Option<&str>) -> bool {
        self.events
            .iter()
            .any(|e| (e == "*" && !PERIODIC_OPS.contains(&op)) || e == op || kind.is_some_and(|k| e == k))
    }
}

/// Where a plugin track's audio comes from (`resolve`).
#[derive(Debug, Clone, PartialEq)]
pub enum PlayTarget {
    Url {
        url: String,
        headers: HashMap<String, String>,
        /// Container hint (`mp3`, `webm`, ...).
        format: Option<String>,
    },
    File {
        path: PathBuf,
    },
    /// An identifier for the node to load (`ytsearch:artist - title`).
    Load(String),
    Track(TrackInfo),
}

/// A plugin's answer to `route`, turned into an HTTP response by the API layer.
#[derive(Debug, Clone)]
pub struct RouteResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

pub struct PluginHost {
    plugins: Vec<Arc<PluginProcess>>,
    request_timeout: Duration,
    startup_timeout: Duration,
    init_node: Value,
    node: Arc<OnceLock<Weak<NodeContext>>>,
}

impl PluginHost {
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            plugins: Vec::new(),
            request_timeout: Duration::from_secs(10),
            startup_timeout: Duration::from_secs(15),
            init_node: Value::Null,
            node: Arc::default(),
        })
    }

    pub fn from_config(config: &AppConfig) -> anyhow::Result<Arc<Self>> {
        let cfg = &config.plugins;
        if !cfg.enabled {
            return Ok(Self::disabled());
        }
        let plugins = definitions(cfg)?.into_iter().map(PluginProcess::new).collect();
        Ok(Arc::new(Self {
            plugins,
            request_timeout: Duration::from_secs(cfg.request_timeout_secs.max(1)),
            startup_timeout: Duration::from_secs(cfg.startup_timeout_secs.max(1)),
            init_node: json!({
                "name": "rust-link",
                "version": env!("CARGO_PKG_VERSION"),
                "lavalinkApi": crate::api::info_routes::LAVALINK_API_VERSION,
                "address": config.server.address,
                "port": config.server.port,
            }),
            node: Arc::default(),
        }))
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    pub fn plugins(&self) -> &[Arc<PluginProcess>] {
        &self.plugins
    }

    /// Start every plugin and wait for their first handshake (bounded by the startup
    /// timeout); plugins that fail keep retrying in the background.
    pub async fn start(&self, shutdown: CancellationToken) {
        if self.plugins.is_empty() {
            return;
        }
        let handler = inbound_handler(Arc::clone(&self.node));
        for plugin in &self.plugins {
            let params = json!({
                "protocol": PROTOCOL_VERSION,
                "name": plugin.name(),
                "config": plugin.spec.config,
                "node": self.init_node,
            });
            plugin.spawn_supervisor(Arc::clone(&handler), params, self.startup_timeout, shutdown.clone());
        }
        let all_started = futures_util::future::join_all(self.plugins.iter().map(|p| p.wait_started()));
        if tokio::time::timeout(self.startup_timeout + Duration::from_secs(1), all_started)
            .await
            .is_err()
        {
            warn!("some plugins are still starting");
        }
        let running = self
            .plugins
            .iter()
            .filter(|p| p.state() == PluginState::Running)
            .count();
        info!(running, configured = self.plugins.len(), "plugins started");
    }

    /// Gives plugins access to the node (`rest` calls).
    pub fn attach_node(&self, ctx: &Arc<NodeContext>) {
        let _ = self.node.set(Arc::downgrade(ctx));
    }

    fn with_manifest(&self) -> impl Iterator<Item = (&Arc<PluginProcess>, Arc<Manifest>)> {
        self.plugins.iter().filter_map(|p| p.manifest().map(|m| (p, m)))
    }

    /// `/v4/info` plugin list.
    pub fn info_list(&self) -> Vec<models::Plugin> {
        self.with_manifest()
            .map(|(p, m)| models::Plugin {
                name: p.name().to_string(),
                version: m.version.clone(),
            })
            .collect()
    }

    pub fn source_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for (_, manifest) in self.with_manifest() {
            for name in &manifest.source_names {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
        names
    }

    /// `/v4/rustlink/plugins`.
    pub fn status(&self) -> Value {
        Value::Array(
            self.plugins
                .iter()
                .map(|p| {
                    let manifest = p.manifest();
                    json!({
                        "name": p.name(),
                        "state": p.state(),
                        "restarts": p.restarts(),
                        "lastError": p.last_error(),
                        "command": p.spec.command,
                        "version": manifest.as_ref().map(|m| m.version.clone()),
                        "sourceNames": manifest.as_ref().map(|m| m.source_names.clone()).unwrap_or_default(),
                        "searchPrefixes": manifest.as_ref().map(|m| m.search_prefixes.clone()).unwrap_or_default(),
                        "urlPatterns": manifest
                            .as_ref()
                            .map(|m| m.url_patterns.iter().map(|r| r.as_str().to_string()).collect::<Vec<_>>())
                            .unwrap_or_default(),
                        "events": manifest.as_ref().map(|m| m.events.clone()).unwrap_or_default(),
                        "routes": manifest
                            .as_ref()
                            .map(|m| m.routes.iter().map(|r| format!("{} {}", r.method, r.path)).collect::<Vec<_>>())
                            .unwrap_or_default(),
                        "resolve": manifest.as_ref().is_some_and(|m| m.resolve),
                        "cache": manifest.as_ref().is_none_or(|m| m.cache),
                    })
                })
                .collect(),
        )
    }

    /// Track loading through the first plugin that claims the identifier. `None` when no
    /// plugin claims it, or every claiming plugin answered `null` (not mine after all).
    pub async fn load_item(&self, identifier: &str) -> Option<LoadResult> {
        for (plugin, manifest) in self.with_manifest() {
            let Some(params) = manifest.claim(identifier) else {
                continue;
            };
            debug!(plugin = plugin.name(), identifier, "loading through plugin");
            return match plugin.request("loadItem", params, self.request_timeout).await {
                Ok(Value::Null) => continue,
                Ok(value) => Some(
                    parse_load_result(value, &manifest.default_source(plugin.name())).unwrap_or_else(|e| {
                        LoadResult::error(format!("plugin {}: {e}", plugin.name()), Severity::Fault)
                    }),
                ),
                Err(e) => Some(LoadResult::error(e.to_string(), Severity::Common)),
            };
        }
        None
    }

    fn owner(&self, source_name: &str) -> Option<(&Arc<PluginProcess>, Arc<Manifest>)> {
        self.with_manifest()
            .find(|(_, m)| m.source_names.iter().any(|s| s == source_name))
    }

    pub fn owns_source(&self, source_name: &str) -> bool {
        self.owner(source_name).is_some()
    }

    /// Tracks of plugins with `cache: false` are always played straight from their origin.
    pub fn bypasses_cache(&self, info: &TrackInfo) -> bool {
        self.owner(&info.source_name).is_some_and(|(_, m)| !m.cache)
    }

    /// Playback target of a plugin track. `Ok(None)` when the track is not a plugin's, or
    /// the plugin leaves playback to the node (its `uri` is then used).
    pub async fn resolve(&self, info: &TrackInfo) -> Result<Option<PlayTarget>, PluginError> {
        let Some((plugin, manifest)) = self.owner(&info.source_name) else {
            return Ok(None);
        };
        if !manifest.resolve {
            return Ok(None);
        }
        let value = plugin
            .request("resolve", json!({"track": info}), self.request_timeout)
            .await?;
        parse_target(value, &manifest.default_source(plugin.name())).map_err(|message| PluginError::Remote {
            plugin: plugin.name().to_string(),
            code: INVALID_PARAMS,
            message,
        })
    }

    /// Forward a websocket message to the plugins that subscribed to it.
    pub fn dispatch(&self, session_id: &str, message: &OutgoingMessage) {
        if self.plugins.is_empty() {
            return;
        }
        let mut value: Option<Value> = None;
        for (plugin, manifest) in self.with_manifest() {
            if manifest.events.is_empty() {
                continue;
            }
            let value = value.get_or_insert_with(|| serde_json::to_value(message).unwrap_or_default());
            let op = value.get("op").and_then(Value::as_str).unwrap_or_default();
            let kind = value.get("type").and_then(Value::as_str);
            if manifest.subscribes(op, kind) {
                plugin.notify(
                    "event",
                    json!({
                        "sessionId": session_id,
                        "guildId": value.get("guildId"),
                        "message": value,
                    }),
                );
            }
        }
    }

    /// The plugin, its matching route (`GET /v4/x/{id}`) and the path parameters.
    pub fn find_route(&self, method: &str, path: &str) -> Option<(Arc<PluginProcess>, String, Map<String, Value>)> {
        self.with_manifest().find_map(|(plugin, manifest)| {
            manifest.routes.iter().find_map(|route| {
                route
                    .matches(method, path)
                    .map(|params| (Arc::clone(plugin), format!("{} {}", route.method, route.path), params))
            })
        })
    }

    pub async fn call_route(&self, plugin: &PluginProcess, request: Value) -> Result<RouteResponse, PluginError> {
        let value = plugin.request("route", request, self.request_timeout).await?;
        Ok(parse_route_response(value))
    }
}

/// Language of a new plugin skeleton.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Python,
    Node,
}

const PYTHON_SDK: &str = include_str!("../../sdk/python/rustlink_plugin.py");
const NODE_SDK: &str = include_str!("../../sdk/node/rustlink-plugin.js");
const PYTHON_TEMPLATE: &str = include_str!("../../sdk/templates/main.py");
const NODE_TEMPLATE: &str = include_str!("../../sdk/templates/main.js");

/// Create `<directory>/<name>/` with a `plugin.yml`, a starter plugin and the SDK.
pub fn scaffold(directory: &Path, name: &str, language: Language) -> anyhow::Result<PathBuf> {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !valid {
        bail!("plugin names use lower case letters, digits, - and _ (got {name:?})");
    }
    if PLUGIN_HOST_KEYS.contains(&name) {
        bail!("plugin name {name:?} is reserved");
    }
    let target = directory.join(name);
    if target.exists() {
        bail!("{} already exists", target.display());
    }
    std::fs::create_dir_all(&target).with_context(|| format!("cannot create {}", target.display()))?;
    let python = if cfg!(windows) { "python" } else { "python3" };
    let (command, files) = match language {
        Language::Python => (
            format!("[\"{python}\", \"main.py\"]"),
            [("main.py", PYTHON_TEMPLATE), ("rustlink_plugin.py", PYTHON_SDK)],
        ),
        Language::Node => (
            "[\"node\", \"main.js\"]".to_string(),
            [("main.js", NODE_TEMPLATE), ("rustlink-plugin.js", NODE_SDK)],
        ),
    };
    let manifest = format!(
        "# Started by rust-link from this folder. Settings for the plugin go under\n\
         # `plugins.{name}` in application.yml.\n\
         name: {name}\n\
         command: {command}\n\
         enabled: true\n\
         # env:\n\
         #   API_KEY: \"...\"\n"
    );
    std::fs::write(target.join("plugin.yml"), manifest)?;
    for (file, content) in files {
        std::fs::write(target.join(file), content.replace("__NAME__", name))?;
    }
    Ok(target)
}

/// `plugins.directory/*/plugin.yml` plus `plugins.entries`.
pub fn definitions(cfg: &PluginsConfig) -> anyhow::Result<Vec<LaunchSpec>> {
    let mut found: Vec<(PluginEntry, PathBuf)> = Vec::new();
    if cfg.directory.is_dir() {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&cfg.directory)
            .with_context(|| format!("cannot read plugin directory {}", cfg.directory.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let Some(file) = MANIFEST_FILES.iter().map(|f| dir.join(f)).find(|f| f.is_file()) else {
                continue;
            };
            // One broken plugin folder must not keep the node from starting.
            let parsed = std::fs::read_to_string(&file)
                .map_err(anyhow::Error::from)
                .and_then(|raw| Ok(serde_norway::from_str::<PluginEntry>(&raw)?));
            let mut entry = match parsed {
                Ok(entry) => entry,
                Err(e) => {
                    warn!(file = %file.display(), error = %e, "plugin skipped: invalid plugin file");
                    continue;
                }
            };
            if entry.name.trim().is_empty() {
                entry.name = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
            }
            let working_dir = match &entry.working_dir {
                Some(sub) => dir.join(sub),
                None => dir.clone(),
            };
            found.push((entry, working_dir));
        }
    }
    for entry in &cfg.entries {
        let working_dir = entry.working_dir.clone().unwrap_or_else(|| PathBuf::from("."));
        found.push((entry.clone(), working_dir));
    }

    let mut names = HashSet::new();
    let mut specs = Vec::new();
    for (entry, working_dir) in found {
        let name = entry.name.trim().to_string();
        if !entry.enabled {
            info!(plugin = %name, "plugin disabled");
            continue;
        }
        if name.is_empty() {
            bail!("a plugin in plugins.entries has no name");
        }
        if PLUGIN_HOST_KEYS.contains(&name.as_str()) {
            bail!("plugin name {name:?} is reserved");
        }
        if entry.command.is_empty() {
            bail!("plugin {name} has no command");
        }
        if !names.insert(name.clone()) {
            bail!("plugin {name} is defined twice");
        }
        let config = entry
            .config
            .clone()
            .or_else(|| cfg.settings.get(&name).cloned())
            .unwrap_or_else(|| json!({}));
        specs.push(LaunchSpec {
            name,
            command: entry.command.clone(),
            working_dir: absolute(&working_dir),
            env: entry.env.clone(),
            config,
        });
    }
    Ok(specs)
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn inbound_handler(node: Arc<OnceLock<Weak<NodeContext>>>) -> InboundHandler {
    Arc::new(move |plugin, method, params| {
        let node = node.get().and_then(Weak::upgrade);
        Box::pin(async move {
            let ctx = node.ok_or((-32000, "node is not ready".to_string()))?;
            match method.as_str() {
                "rest" => rest(&ctx, &plugin, params).await,
                _ => Err((METHOD_NOT_FOUND, format!("unknown method {method}"))),
            }
        })
    })
}

/// A plugin calling the node's REST API: `{method, path, body?}` → `{status, body}`.
async fn rest(ctx: &Arc<NodeContext>, plugin: &str, params: Value) -> Result<Value, (i64, String)> {
    let method = params.get("method").and_then(Value::as_str).unwrap_or("GET");
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| p.starts_with('/'))
        .ok_or((INVALID_PARAMS, "rest needs a path starting with /".to_string()))?;
    let body = params.get("body").filter(|b| !b.is_null());
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header(axum::http::header::AUTHORIZATION, &ctx.config.server.password);
    if body.is_some() {
        request = request.header(axum::http::header::CONTENT_TYPE, "application/json");
    }
    let request = request
        .body(match body {
            Some(body) => Body::from(serde_json::to_vec(body).unwrap_or_default()),
            None => Body::empty(),
        })
        .map_err(|e| (INVALID_PARAMS, format!("invalid request: {e}")))?;
    debug!(plugin, method, path, "plugin REST call");
    let response = crate::api::router(Arc::clone(ctx))
        .oneshot(request)
        .await
        .map_err(|e| (-32000, e.to_string()))?;
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), REST_BODY_LIMIT)
        .await
        .map_err(|e| (-32000, e.to_string()))?;
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    Ok(json!({"status": status, "body": body}))
}

/// A plugin track: a Lavalink track object, or just its `info`, with missing fields
/// filled in. The `encoded` string is always produced by the node.
pub fn parse_track(value: &Value, default_source: &str) -> Result<Track, String> {
    let info = value.get("info").filter(|i| i.is_object()).unwrap_or(value);
    if !info.is_object() {
        return Err(format!("expected a track object, got {value}"));
    }
    let text = |key: &str| info.get(key).and_then(Value::as_str).map(str::to_string);
    let uri = text("uri");
    let identifier = text("identifier")
        .or_else(|| uri.clone())
        .ok_or("track without identifier or uri")?;
    let is_stream = info.get("isStream").and_then(Value::as_bool).unwrap_or(false);
    let length = info
        .get("length")
        .and_then(Value::as_u64)
        .unwrap_or(if is_stream { STREAM_LENGTH } else { 0 });
    let track = TrackInfo {
        is_seekable: info.get("isSeekable").and_then(Value::as_bool).unwrap_or(!is_stream),
        author: text("author").unwrap_or_else(|| "Unknown artist".into()),
        length,
        is_stream,
        position: info.get("position").and_then(Value::as_u64).unwrap_or(0),
        title: text("title").unwrap_or_else(|| identifier.clone()),
        uri,
        artwork_url: text("artworkUrl"),
        isrc: text("isrc"),
        source_name: text("sourceName").unwrap_or_else(|| default_source.to_string()),
        identifier,
    };
    let plugin_info = value
        .get("pluginInfo")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut track = Track::with_plugin_info(track, plugin_info);
    if let Some(user_data) = value.get("userData").and_then(Value::as_object) {
        track.user_data = user_data.clone();
    }
    Ok(track)
}

fn parse_tracks(value: &Value, default_source: &str) -> Result<Vec<Track>, String> {
    value
        .as_array()
        .ok_or_else(|| format!("expected a list of tracks, got {value}"))?
        .iter()
        .map(|t| parse_track(t, default_source))
        .collect()
}

fn search_or_empty(tracks: Vec<Track>) -> LoadResult {
    if tracks.is_empty() {
        LoadResult::empty()
    } else {
        LoadResult::Search(tracks)
    }
}

/// `loadItem` answer: a Lavalink load result (`{loadType, data}`), or a shorthand: a list
/// of tracks (search), `{tracks, name?}` (playlist when named) or a single track.
pub fn parse_load_result(value: Value, default_source: &str) -> Result<LoadResult, String> {
    if value.is_array() {
        return Ok(search_or_empty(parse_tracks(&value, default_source)?));
    }
    let null = Value::Null;
    if let Some(load_type) = value.get("loadType").and_then(Value::as_str) {
        let data = value.get("data").unwrap_or(&null);
        return Ok(match load_type {
            "track" => LoadResult::Track(parse_track(data, default_source)?),
            "search" => search_or_empty(parse_tracks(data, default_source)?),
            "playlist" => playlist(data, default_source)?,
            "empty" => LoadResult::empty(),
            "error" => {
                let message = data
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("plugin error")
                    .to_string();
                let severity = data
                    .get("severity")
                    .and_then(|s| serde_json::from_value(s.clone()).ok())
                    .unwrap_or(Severity::Common);
                LoadResult::error(message, severity)
            }
            other => return Err(format!("unknown loadType {other:?}")),
        });
    }
    if let Some(tracks) = value.get("tracks") {
        if value.get("name").is_some() || value.get("info").is_some() {
            return playlist(&value, default_source);
        }
        return Ok(search_or_empty(parse_tracks(tracks, default_source)?));
    }
    Ok(LoadResult::Track(parse_track(&value, default_source)?))
}

fn playlist(data: &Value, default_source: &str) -> Result<LoadResult, String> {
    let info = data.get("info").unwrap_or(data);
    let tracks = parse_tracks(data.get("tracks").unwrap_or(&Value::Null), default_source)?;
    if tracks.is_empty() {
        return Ok(LoadResult::empty());
    }
    Ok(LoadResult::Playlist(PlaylistData {
        info: PlaylistInfo {
            name: info
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Playlist")
                .to_string(),
            selected_track: info
                .get("selectedTrack")
                .and_then(Value::as_i64)
                .and_then(|i| i32::try_from(i).ok())
                .unwrap_or(-1),
        },
        plugin_info: data
            .get("pluginInfo")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        tracks,
    }))
}

/// `resolve` answer: `null` (use the track URI), `{url, headers?, format?}`,
/// `{file}`, `{identifier}`, `{track}`, or a bare string (URL, absolute path or
/// identifier).
pub fn parse_target(value: Value, default_source: &str) -> Result<Option<PlayTarget>, String> {
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    Ok(Some(match &value {
        Value::Null => return Ok(None),
        Value::String(s) => {
            if s.starts_with("http://") || s.starts_with("https://") {
                PlayTarget::Url {
                    url: s.clone(),
                    headers: HashMap::new(),
                    format: None,
                }
            } else if let Some(path) = s.strip_prefix("file://") {
                PlayTarget::File { path: path.into() }
            } else if Path::new(s).is_absolute() {
                PlayTarget::File { path: s.into() }
            } else {
                PlayTarget::Load(s.clone())
            }
        }
        Value::Object(_) => {
            if let Some(url) = text(&value, "url") {
                let headers = value
                    .get("headers")
                    .and_then(Value::as_object)
                    .map(|h| {
                        h.iter()
                            .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();
                PlayTarget::Url {
                    url,
                    headers,
                    format: text(&value, "format"),
                }
            } else if let Some(path) = text(&value, "file").or_else(|| text(&value, "path")) {
                PlayTarget::File { path: path.into() }
            } else if let Some(identifier) = text(&value, "identifier") {
                PlayTarget::Load(identifier)
            } else if let Some(track) = value.get("track") {
                PlayTarget::Track(parse_track(track, default_source)?.info)
            } else {
                return Err(format!("resolve answer not understood: {value}"));
            }
        }
        other => return Err(format!("resolve answer not understood: {other}")),
    }))
}

/// `route` answer: an object is always the envelope `{status?, headers?, body?}`; any
/// other value (string, list, number, `null`) is the body itself.
fn parse_route_response(value: Value) -> RouteResponse {
    if !value.is_object() {
        return RouteResponse {
            status: if value.is_null() { 204 } else { 200 },
            headers: Vec::new(),
            body: value,
        };
    }
    let body = value.get("body").cloned().unwrap_or(Value::Null);
    RouteResponse {
        status: value
            .get("status")
            .and_then(Value::as_u64)
            .and_then(|s| u16::try_from(s).ok())
            .unwrap_or(if body.is_null() { 204 } else { 200 }),
        headers: value
            .get("headers")
            .and_then(Value::as_object)
            .map(|h| {
                h.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                    .collect()
            })
            .unwrap_or_default(),
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_match_params_and_rest() {
        let route = RouteSpec::parse("get", "/v4/lyrics/{guildId}").unwrap();
        let params = route.matches("GET", "/v4/lyrics/123").unwrap();
        assert_eq!(params["guildId"], "123");
        assert!(route.matches("POST", "/v4/lyrics/123").is_none());
        assert!(route.matches("GET", "/v4/lyrics").is_none());
        assert!(route.matches("GET", "/v4/lyrics/1/2").is_none());

        let rest = RouteSpec::parse("*", "/v4/files/{*path}").unwrap();
        let params = rest.matches("DELETE", "/v4/files/a/b%20c.mp3").unwrap();
        assert_eq!(params["path"], "a/b c.mp3");
        assert!(RouteSpec::parse("GET", "/v4/{*a}/x").is_err());
        assert!(RouteSpec::parse("GET", "v4/x").is_err());
    }

    #[test]
    fn manifest_claims_prefixes_and_urls() {
        let manifest = Manifest::parse(
            json!({
                "version": "1.2.3",
                "sourceName": "radio",
                "searchPrefixes": ["Radio:", "fm"],
                "urlPatterns": ["^https?://radio\\.example/"],
                "events": ["TrackStartEvent", "*"],
                "routes": [{"method": "GET", "path": "/v4/radio"}],
                "resolve": true,
                "cache": false
            }),
            "radio",
        )
        .unwrap();
        assert_eq!(manifest.search_prefixes, ["radio", "fm"]);
        let claim = manifest.claim("RADIO: lofi beats").unwrap();
        assert_eq!(claim["prefix"], "radio");
        assert_eq!(claim["query"], "lofi beats");
        assert_eq!(
            manifest.claim("https://radio.example/x").unwrap()["url"],
            "https://radio.example/x"
        );
        assert!(manifest.claim("ytsearch:lofi").is_none());
        assert!(manifest.subscribes("event", Some("TrackEndEvent")), "* covers events");
        assert!(
            !manifest.subscribes("playerUpdate", None),
            "periodic updates must be named"
        );
        assert!(Manifest::parse(json!({"urlPatterns": ["("]}), "x").is_err());
        assert!(Manifest::parse(Value::Null, "bare").unwrap().cache);
    }

    #[test]
    fn load_results_accept_lavalink_and_shorthand() {
        let full = parse_load_result(
            json!({"loadType": "track", "data": {"info": {"identifier": "a", "title": "A"}, "pluginInfo": {"x": 1}}}),
            "radio",
        )
        .unwrap();
        let LoadResult::Track(track) = full else {
            panic!("expected a track")
        };
        assert_eq!(track.info.source_name, "radio");
        assert_eq!(track.plugin_info["x"], 1);
        assert_eq!(
            crate::api::track_codec::decode_track(&track.encoded)
                .unwrap()
                .identifier,
            "a"
        );

        let search = parse_load_result(json!([{"identifier": "1"}, {"uri": "https://x/2.mp3"}]), "s").unwrap();
        let LoadResult::Search(tracks) = search else {
            panic!("expected search results")
        };
        assert_eq!(tracks[1].info.identifier, "https://x/2.mp3");

        let playlist = parse_load_result(
            json!({"name": "Mix", "selectedTrack": 1, "tracks": [{"identifier": "1"}, {"identifier": "2"}]}),
            "s",
        )
        .unwrap();
        let LoadResult::Playlist(p) = playlist else {
            panic!("expected a playlist")
        };
        assert_eq!((p.info.name.as_str(), p.info.selected_track), ("Mix", 1));

        let live = parse_load_result(json!({"identifier": "fm", "isStream": true}), "s").unwrap();
        let LoadResult::Track(t) = live else {
            panic!("expected a track")
        };
        assert_eq!(t.info.length, STREAM_LENGTH);
        assert!(!t.info.is_seekable);

        assert!(matches!(
            parse_load_result(json!({"loadType": "search", "data": []}), "s").unwrap(),
            LoadResult::Empty(_)
        ));
        assert!(matches!(
            parse_load_result(json!({"loadType": "error", "data": {"message": "nope"}}), "s").unwrap(),
            LoadResult::Error(_)
        ));
        assert!(parse_load_result(json!({"title": "no id"}), "s").is_err());
    }

    #[test]
    fn resolve_answers() {
        assert_eq!(parse_target(Value::Null, "s").unwrap(), None);
        assert!(matches!(
            parse_target(json!({"url": "https://cdn/x.mp3", "headers": {"Referer": "r"}, "format": "mp3"}), "s"),
            Ok(Some(PlayTarget::Url { format: Some(f), .. })) if f == "mp3"
        ));
        assert_eq!(
            parse_target(json!("ytsearch:a b"), "s").unwrap(),
            Some(PlayTarget::Load("ytsearch:a b".into()))
        );
        assert!(matches!(
            parse_target(json!({"file": "/music/a.flac"}), "s"),
            Ok(Some(PlayTarget::File { .. }))
        ));
        assert!(matches!(
            parse_target(json!({"track": {"identifier": "dQw4w9WgXcQ", "sourceName": "youtube"}}), "s"),
            Ok(Some(PlayTarget::Track(t))) if t.source_name == "youtube"
        ));
        assert!(parse_target(json!({"nothing": 1}), "s").is_err());
    }

    #[test]
    fn route_responses() {
        let list = parse_route_response(json!(["a", "b"]));
        assert_eq!((list.status, list.body[1].as_str()), (200, Some("b")));
        let envelope = parse_route_response(json!({"status": 404, "body": "missing"}));
        assert_eq!((envelope.status, envelope.body.as_str()), (404, Some("missing")));
        let body = parse_route_response(json!({"body": {"status": "ok"}, "headers": {"x-a": "1"}}));
        assert_eq!((body.status, body.body["status"].as_str()), (200, Some("ok")));
        assert_eq!(body.headers, [("x-a".to_string(), "1".to_string())]);
        assert_eq!(parse_route_response(Value::Null).status, 204);
        assert_eq!(parse_route_response(json!({})).status, 204);
    }
}
