'use strict';
/**
 * rust-link plugin SDK for Node.js 16+ (no dependencies).
 *
 * Copy this file next to your plugin (`rust-link plugin new <name> --lang node` does it):
 *
 *   const { Plugin, track } = require('./rustlink-plugin');
 *   const plugin = new Plugin({ version: '1.0.0', sourceName: 'radio', cache: false });
 *   plugin.search('radio', async (query) => [
 *     track({ identifier: 'lofi', title: 'Lofi radio', uri: 'https://radio.example/lofi.mp3', isStream: true }),
 *   ]);
 *   plugin.run();
 *
 * The node starts the plugin, talks JSON-RPC over stdin/stdout and logs whatever the
 * plugin writes to stderr. console.log is redirected to stderr, so it is safe to use.
 */
const readline = require('readline');

const PERIODIC = ['playerUpdate', 'stats'];

class PluginError extends Error {
  constructor(message, code = -32000) {
    super(message);
    this.code = code;
  }
}

/** A track for a load result; missing fields are filled in by the node. */
function track({ identifier, title, author, length, uri, isStream, isSeekable, artworkUrl, isrc, sourceName,
  pluginInfo, userData } = {}) {
  const info = { identifier, title, author, length, uri, isStream, isSeekable, artworkUrl, isrc, sourceName };
  for (const key of Object.keys(info)) if (info[key] === undefined || info[key] === null) delete info[key];
  const result = { info };
  if (pluginInfo) result.pluginInfo = pluginInfo;
  if (userData) result.userData = userData;
  return result;
}

function playlist(name, tracks, selected = -1, pluginInfo = {}) {
  return { loadType: 'playlist', data: { info: { name, selectedTrack: selected }, pluginInfo, tracks } };
}

function error(message, severity = 'common') {
  return { loadType: 'error', data: { message, severity } };
}

/** A route answer with a status code and headers: reply(body, { status: 201 }). */
class Reply {
  constructor(body = null, { status, headers = {} } = {}) {
    this.body = body;
    this.status = status;
    this.headers = headers;
  }

  envelope() {
    const result = { body: this.body, headers: this.headers };
    if (this.status !== undefined) result.status = this.status;
    return result;
  }
}

const reply = (body, options) => new Reply(body, options);

const routeKey = (method, path) => `${method.toUpperCase()} ${path}`;

class Plugin {
  constructor({ name, version = '0.1.0', sourceName, cache = true } = {}) {
    this.name = name || process.env.RUSTLINK_PLUGIN || 'plugin';
    this.version = version;
    this.sourceName = sourceName;
    this.cache = cache;
    /** Settings from `plugins.<name>` in application.yml (filled at startup). */
    this.config = {};
    /** Node details: name, version, lavalinkApi, address, port. */
    this.node = {};
    this._prefixes = new Map();
    this._urls = [];
    this._resolver = null;
    this._events = new Map();
    this._routes = new Map();
    this._onStart = [];
    this._onStop = [];
    this._nextId = 1;
    this._pending = new Map();
    this._eventChain = Promise.resolve();
    const toStderr = (...args) => console.error(...args);
    console.log = toStderr;
    console.info = toStderr;
    console.debug = toStderr;
  }

  /** Handle `prefix:query` identifiers; the handler gets (query, identifier). */
  search(prefixes, handler) {
    for (const prefix of [].concat(prefixes)) this._prefixes.set(prefix.trim().replace(/:$/, '').toLowerCase(), handler);
    return this;
  }

  /** Handle identifiers matching a RegExp (keep to syntax Rust's regex shares); handler gets (url, match). */
  url(pattern, handler) {
    const regex = pattern instanceof RegExp ? pattern : new RegExp(pattern);
    this._urls.push({ regex, handler });
    return this;
  }

  /** Where the plugin's tracks play from: {url, headers?}, {file}, {identifier}, {track} or null. */
  resolver(handler) {
    this._resolver = handler;
    return this;
  }

  /** Websocket messages: event types, ops (ready, playerUpdate, stats) or '*'; handler gets (message, sessionId). */
  on(names, handler) {
    for (const name of [].concat(names)) {
      if (!this._events.has(name)) this._events.set(name, []);
      this._events.get(name).push(handler);
    }
    return this;
  }

  /**
   * REST route such as ('GET', '/v4/lyrics/{guildId}'). The handler gets a request object
   * and returns the body (a string is sent as text) or reply(body, { status, headers }).
   */
  route(method, path, handler) {
    this._routes.set(routeKey(method, path), { method: method.toUpperCase(), path, handler });
    return this;
  }

  onStart(handler) {
    this._onStart.push(handler);
    return this;
  }

  onStop(handler) {
    this._onStop.push(handler);
    return this;
  }

  log(message, level = 'info') {
    this._send({ jsonrpc: '2.0', method: 'log', params: { level, message: String(message) } });
  }

  /** Call the node's REST API (no password needed); resolves to {status, body}. */
  async rest(method, path, { body, query, timeoutMs = 30000 } = {}) {
    if (query) path += (path.includes('?') ? '&' : '?') + new URLSearchParams(query).toString();
    return this._request('rest', { method, path, body: body === undefined ? null : body }, timeoutMs);
  }

  /** /v4/loadtracks through the node: any source, including other plugins. */
  async load(identifier) {
    const { status, body } = await this.rest('GET', '/v4/loadtracks', { query: { identifier } });
    if (status !== 200) throw new PluginError(`loadtracks failed with status ${status}`);
    return body;
  }

  manifest() {
    return {
      name: this.name,
      version: this.version,
      sourceName: this.sourceName,
      searchPrefixes: [...this._prefixes.keys()].sort(),
      urlPatterns: this._urls.map(({ regex }) => regex.source),
      events: [...this._events.keys()].sort(),
      routes: [...this._routes.values()].map(({ method, path }) => ({ method, path })),
      resolve: this._resolver !== null,
      cache: this.cache,
    };
  }

  _send(message) {
    process.stdout.write(JSON.stringify(message) + '\n');
  }

  _request(method, params, timeoutMs) {
    const id = this._nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this._pending.delete(id);
        reject(new PluginError(`node did not answer ${method} within ${timeoutMs} ms`));
      }, timeoutMs);
      this._pending.set(id, { resolve, reject, timer });
      this._send({ jsonrpc: '2.0', id, method, params });
    });
  }

  async _handle(method, params) {
    switch (method) {
      case 'initialize':
        this.config = params.config || {};
        this.node = params.node || {};
        for (const fn of this._onStart) await fn(this.config, this.node);
        return this.manifest();
      case 'loadItem': {
        const identifier = params.identifier || '';
        if (params.prefix !== undefined && this._prefixes.has(params.prefix)) {
          return (await this._prefixes.get(params.prefix)(params.query || '', identifier)) ?? null;
        }
        for (const { regex, handler } of this._urls) {
          const match = identifier.match(regex);
          if (match) return (await handler(identifier, match)) ?? null;
        }
        return null;
      }
      case 'resolve':
        return this._resolver ? (await this._resolver(params.track || {})) ?? null : null;
      case 'route': {
        const entry = this._routes.get(params.route || '');
        if (!entry) throw new PluginError(`no handler for ${params.route}`, -32601);
        const answer = await entry.handler({
          route: params.route,
          method: params.method,
          path: params.path,
          params: params.params || {},
          query: params.query || {},
          headers: params.headers || {},
          body: params.body ?? null,
        });
        return answer instanceof Reply ? answer.envelope() : { body: answer ?? null };
      }
      default:
        throw new PluginError(`unknown method ${method}`, -32601);
    }
  }

  _dispatchEvent(params) {
    const message = params.message || {};
    const handlers = [message.type, message.op]
      .filter(Boolean)
      .flatMap((name) => this._events.get(name) || []);
    if (!PERIODIC.includes(message.op)) handlers.push(...(this._events.get('*') || []));
    // Keep events in order, one after the other.
    this._eventChain = this._eventChain.then(async () => {
      for (const fn of handlers) {
        try {
          await fn(message, params.sessionId);
        } catch (e) {
          console.error(e);
        }
      }
    });
  }

  async _stop() {
    for (const fn of this._onStop) {
      try {
        await fn();
      } catch (e) {
        console.error(e);
      }
    }
    process.exit(0);
  }

  /** Serve the node until it closes stdin or asks the plugin to stop. */
  run() {
    const lines = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
    lines.on('line', (line) => {
      line = line.trim();
      if (!line) return;
      let message;
      try {
        message = JSON.parse(line);
      } catch (e) {
        console.error(`rust-link sent invalid JSON: ${line.slice(0, 200)}`);
        return;
      }
      const { method, id } = message;
      if (method === undefined) {
        const slot = this._pending.get(id);
        if (!slot) return;
        this._pending.delete(id);
        clearTimeout(slot.timer);
        if (message.error) slot.reject(new PluginError(message.error.message || 'node error', message.error.code));
        else slot.resolve(message.result || {});
      } else if (method === 'shutdown') {
        this._stop();
      } else if (method === 'event') {
        this._dispatchEvent(message.params || {});
      } else if (id !== undefined && id !== null) {
        this._handle(method, message.params || {}).then(
          (result) => this._send({ jsonrpc: '2.0', id, result: result ?? null }),
          (e) => {
            if (!(e instanceof PluginError)) console.error(e);
            this._send({ jsonrpc: '2.0', id, error: { code: e.code || -32000, message: e.message || String(e) } });
          },
        );
      }
    });
    lines.on('close', () => this._stop());
  }
}

module.exports = { Plugin, PluginError, Reply, reply, track, playlist, error };
