// driver.mjs — self-contained raw-CDP browser driver for the cf-cloak solver.
//
// PURPOSE
//   A minimal raw-CDP driver over the `ws` package with hand-rolled JSON
//   framing: flat sessions, browser contexts, pages, Fetch interception, and
//   proxy auth. It provides exactly the slice the consumer (server.mjs)
//   actually uses. The public API (method names / signatures /
//   return shapes) is kept stable so the engine needs (almost) zero changes:
//
//     const browser = await launch({ executablePath, args });
//     const ctx     = await browser.createBrowserContext({ proxyServer });
//     const page    = await ctx.newPage();
//     await page.authenticate({ username, password });
//     await page.goto(url, { waitUntil, timeout });
//     await page.evaluate(fn, ...args);
//     await page.waitForFunction(fn, { timeout, polling });
//     page.on('response'|'request'|'console'|'pageerror'|'framenavigated', h);
//     const cdp = await page.target().createCDPSession();  // raw session for Fetch.*
//     await page.cookies(url);
//     ...
//
// ============================================================================
// THE #1 ANTI-DETECTION RULE (non-negotiable):
//   NEVER send `Runtime.enable`. Enabling Runtime makes Chrome eagerly serialize
//   console arguments / exception objects, which anti-bot code detects via the
//   Error.stack getter side-channel (the canonical CDP tell). We ONLY enable
//   `Page`, `Network`, and (on demand) `Fetch` per page. JS is run with
//   `Runtime.evaluate` DIRECTLY — that command works WITHOUT Runtime.enable
//   because it targets the session's default execution context. We also never
//   enable Console, Log, or Debugger.
//
//   Consequence (documented, accepted): the `'console'` and `'pageerror'` page
//   events are wired but effectively NO-OP, because their CDP sources
//   (Runtime.consoleAPICalled / Runtime.exceptionThrown) require Runtime.enable.
//   The engine uses those only for non-load-bearing diagnostics (pageLogs), so
//   losing them is an acceptable approximation. Everything the solver depends on
//   (Network events, Fetch interception, evaluate, cookies) works without Runtime.
// ============================================================================
//
// TRANSPORT DESIGN — flat sessions (chosen over per-page WebSockets):
//   We open ONE WebSocket to the browser-level endpoint and multiplex every
//   target with CDP "flat sessions": attach via `Target.attachToTarget
//   {flatten:true}`, then tag each command with its `sessionId` and route each
//   event by the `sessionId` on the message. This is cleaner than opening a
//   separate ws per `/devtools/page/<id>` because (a) one socket = one place to
//   handle disconnects/backpressure, (b) browser-domain commands and target
//   commands share the same pipe, and (c) it matches Chrome's own flat-session
//   semantics, so `page.target().createCDPSession()` is literally "attach
//   another flat session to the same target" — a second, independent CDP
//   channel into the page.
//
// FETCH COEXISTENCE — THE KEY INTEGRATION RISK (read this):
//   The engine uses Fetch in TWO independent places on the SAME page target:
//     1. `page.authenticate()` — proxy auth (407) handling, and
//     2. `page.target().createCDPSession()` + `Fetch.enable({patterns:[Document]})`
//        in the consumer's arming step, which serves a stub document via
//        Fetch.fulfillRequest. THIS is the most load-bearing path in the whole
//        solver (the "arm").
//   These must not clobber each other. HOW IT ACTUALLY WORKS (see authenticate() below):
//     - authenticate() enables Fetch on the PAGE'S MAIN session with
//       `patterns: [{ urlPattern: '*' }]` (catch-all) + `handleAuthRequests: true`,
//       and its `Fetch.requestPaused` handler IMMEDIATELY `continueRequest`s every
//       paused request. (We cannot use `patterns: []`: Chrome REJECTS an empty array
//       together with `handleAuthRequests` — "Can't specify empty patterns" — which
//       silently broke proxy 407 auth. A catch-all pattern whose pause handler
//       continues every request is the standard CDP idiom for auth-only
//       interception.)
//     - The engine's stub-serving `createCDPSession()` is a SEPARATE flat session
//       with its OWN `Fetch.enable({patterns:[Document]})` on a different sessionId.
//   THE DEPENDENCY (important): both sessions pause the same Document request, but
//   Chrome routes each paused request to exactly ONE session, so the stub session
//   still wins the Document while the main session continue-s everything else. This
//   is load-bearing and relies on Chrome's single-interceptor-per-request routing;
//   it is validated by the live solve passing, not independently re-proven here.
//   Fetch is enabled LAZILY (only when authenticate() needs it), so a proxyless run
//   never arms the main-session interceptor at all.
//
// Allowed imports only: ws, node:child_process, node:http, node:events, node:process.

import WebSocket from 'ws';
import { spawn, spawnSync } from 'node:child_process';
import http from 'node:http';
import { EventEmitter } from 'node:events';
import process from 'node:process';

// Timeout error type. The engine only ever catches it (never checks .name);
// the `TimeoutError` name just makes rejections self-describing.
export class TimeoutError extends Error {
  constructor(message) {
    super(message);
    this.name = 'TimeoutError';
  }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

// Lowercase all keys of a headers object (request/response headers are exposed
// with lowercased keys; consumers read header values with lowercased keys).
function lowerHeaders(h) {
  const out = {};
  if (h) for (const k of Object.keys(h)) out[k.toLowerCase()] = h[k];
  return out;
}

// Build the JS expression string for evaluate/waitForFunction. A function is
// stringified and immediately invoked with JSON-serialized args (plain-JSON
// args go through JSON.stringify); a string is used verbatim.
function evaluationString(fn, ...args) {
  if (typeof fn === 'function') {
    const argList = args.map((a) => JSON.stringify(a === undefined ? null : a)).join(',');
    return `(${fn.toString()})(${argList})`;
  }
  if (args.length) {
    throw new Error('evaluationString: cannot pass args when the first arg is a string expression');
  }
  return String(fn);
}

// waitUntil -> CDP Page.lifecycleEvent name.
function lifecycleName(waitUntil) {
  switch (waitUntil) {
    case 'domcontentloaded':
      return 'DOMContentLoaded';
    case 'networkidle0':
      return 'networkIdle';
    case 'networkidle2':
      return 'networkAlmostIdle';
    case 'load':
    default:
      return 'load';
  }
}

// ---------------------------------------------------------------------------
// Connection — one WebSocket to the browser, id/reply matching + event routing.
// ---------------------------------------------------------------------------
class Connection {
  constructor(ws, protocolTimeout) {
    this._ws = ws;
    this._protocolTimeout = protocolTimeout;
    this._id = 0;
    this._callbacks = new Map(); // id -> { resolve, reject, timer, method }
    this._sessions = new Map();  // sessionId -> CDPSession ('' = browser/root)
    this._closed = false;

    ws.on('message', (data) => this._onMessage(data));
    ws.on('close', () => this._onClose(new Error('WebSocket closed')));
    ws.on('error', (err) => {
      // Never let a socket error crash the process; surface it via close path.
      this._onClose(err instanceof Error ? err : new Error(String(err)));
    });
  }

  // Send a CDP command. sessionId '' (falsy) => browser-level (no sessionId field).
  send(sessionId, method, params = {}) {
    if (this._closed) return Promise.reject(new Error(`Connection closed; cannot send ${method}`));
    if (process.env.CDP_DEBUG) console.error(`[cdp>${sessionId ? sessionId.slice(0, 8) : 'browser'}] ${method}`);
    const id = ++this._id;
    const msg = { id, method, params };
    if (sessionId) msg.sessionId = sessionId;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this._callbacks.delete(id);
        reject(new TimeoutError(`Protocol timeout ${this._protocolTimeout}ms for ${method}`));
      }, this._protocolTimeout);
      timer.unref?.();
      this._callbacks.set(id, { resolve, reject, timer, method });
      try {
        this._ws.send(JSON.stringify(msg));
      } catch (e) {
        clearTimeout(timer);
        this._callbacks.delete(id);
        reject(e);
      }
    });
  }

  rootSend(method, params) {
    return this.send('', method, params);
  }

  _onMessage(data) {
    let obj;
    try {
      obj = JSON.parse(data);
    } catch {
      return; // ignore non-JSON frames (ws handles ping/pong + continuation for us)
    }
    if (obj.id !== undefined) {
      const cb = this._callbacks.get(obj.id);
      if (!cb) return;
      this._callbacks.delete(obj.id);
      clearTimeout(cb.timer);
      if (obj.error) {
        cb.reject(new Error(`${cb.method}: ${obj.error.message || JSON.stringify(obj.error)}`));
      } else {
        cb.resolve(obj.result);
      }
      return;
    }
    // Event (no id): route by sessionId. Missing sessionId => browser/root ('').
    // LEAK FIX: prune a session when its target detaches/destroys. Without this, a context or page
    // closed WITHOUT an explicit CDPSession.detach() (BrowserContext.close only disposes the context)
    // leaves its session in _sessions forever -> unbounded growth on a long-running oracle
    // (every fullPage discovery leaks one each).
    if (obj.method === 'Target.detachedFromTarget' && obj.params?.sessionId) {
      const dead = this._sessions.get(obj.params.sessionId);
      if (dead) { dead._detached = true; this._sessions.delete(obj.params.sessionId); }
    }
    if (process.env.CDP_DEBUG === '2') console.error(`[cdp<${obj.sessionId ? obj.sessionId.slice(0, 8) : 'browser'}] ${obj.method}`);
    const sess = this._sessions.get(obj.sessionId || '');
    if (sess) {
      try {
        sess._emitCdp(obj.method, obj.params || {});
      } catch {
        // a listener throwing must never poison the demux loop
      }
    }
  }

  _onClose(err) {
    if (this._closed) return;
    this._closed = true;
    // Reject every in-flight command so callers don't hang forever.
    for (const cb of this._callbacks.values()) {
      clearTimeout(cb.timer);
      try { cb.reject(err); } catch {}
    }
    this._callbacks.clear();
    // Notify the root session (Browser owns it) so 'disconnected' can fire.
    const root = this._sessions.get('');
    if (root) {
      try { root._emitCdp('__disconnected__', {}); } catch {}
    }
  }

  dispose() {
    try { this._ws.removeAllListeners(); } catch {}
    try { this._ws.close(); } catch {}
    try { this._ws.terminate?.(); } catch {}
    this._onClose(new Error('Connection disposed'));
  }
}

// ---------------------------------------------------------------------------
// CDPSession — a flat session (or the browser root). This is the object returned
// by page.target().createCDPSession(): .send / .on / .off / .detach. Extends
// EventEmitter so CDP events (e.g. 'Fetch.requestPaused') dispatch to listeners.
// ---------------------------------------------------------------------------
class CDPSession extends EventEmitter {
  constructor(connection, sessionId) {
    super();
    this._connection = connection;
    this._sessionId = sessionId || '';
    this._detached = false;
    this.setMaxListeners(0); // the engine attaches several Fetch/Network listeners
  }

  send(method, params = {}) {
    return this._connection.send(this._sessionId, method, params);
  }

  _emitCdp(method, params) {
    this.emit(method, params);
  }

  async detach() {
    if (this._detached) return;
    this._detached = true;
    this._connection._sessions.delete(this._sessionId);
    if (this._sessionId) {
      await this._connection.rootSend('Target.detachFromTarget', { sessionId: this._sessionId }).catch(() => {});
    }
  }
}

// Attach a new flat session to a target and register it for event routing.
async function attachFlatSession(connection, targetId) {
  const { sessionId } = await connection.rootSend('Target.attachToTarget', { targetId, flatten: true });
  const session = new CDPSession(connection, sessionId);
  connection._sessions.set(sessionId, session);
  return session;
}

// ---------------------------------------------------------------------------
// Frame — minimal: url() + reference identity for mainFrame() comparison.
// ---------------------------------------------------------------------------
class Frame {
  constructor(url) {
    this._url = url || '';
  }
  url() {
    return this._url;
  }
}

// ---------------------------------------------------------------------------
// Request / Response wrappers (the slice of the page API the engine consumes).
// ---------------------------------------------------------------------------
class CDPRequest {
  constructor(params) {
    this._url = params.request?.url || '';
    this._method = params.request?.method || '';
    this._headers = lowerHeaders(params.request?.headers);
    this._type = String(params.type || 'Other').toLowerCase(); // Document->document, XHR->xhr, Fetch->fetch...
  }
  url() { return this._url; }
  method() { return this._method; }
  headers() { return this._headers; }
  resourceType() { return this._type; }
}

class CDPResponse {
  constructor(page, params) {
    this._page = page;
    this._requestId = params.requestId;
    this._url = params.response?.url || '';
    this._status = params.response?.status ?? 0;
    this._headers = lowerHeaders(params.response?.headers);
  }
  url() { return this._url; }
  status() { return this._status; }
  headers() { return this._headers; }
  // async text(): waits for the body to finish loading before reading it.
  // We wait on the loadingFinished/loadingFailed promise for this
  // requestId (bounded), then call Network.getResponseBody. Guarded — returns ''
  // on any failure (e.g. body already evicted, or a redirect with no body).
  async text() {
    try {
      await this._page._waitBodyReady(this._requestId, 4000);
      const { body, base64Encoded } = await this._page._session.send('Network.getResponseBody', {
        requestId: this._requestId,
      });
      if (body == null) return '';
      return base64Encoded ? Buffer.from(body, 'base64').toString('utf8') : body;
    } catch {
      return '';
    }
  }
}

// ---------------------------------------------------------------------------
// Page — the big one. Extends EventEmitter so page.on/off expose event subscriptions.
// ---------------------------------------------------------------------------
class Page extends EventEmitter {
  constructor(browser, context, targetId, session) {
    super();
    this._browser = browser;
    this._context = context;
    this._targetId = targetId;
    this._session = session; // the page's MAIN flat session (Page/Network/auth-Fetch)
    this._closed = false;
    this._credentials = null;
    this._authWired = false;
    this._mainFrame = new Frame('about:blank');
    this._mainFrameId = null;
    this._lifecycleSeen = new Map(); // loaderId -> Set(lifecycle names) — for goto pre-check
    this._bodyReady = new Map();     // requestId -> { promise, resolve } (settled on loadingFinished/Failed)
    this.setMaxListeners(0);
  }

  static async _create(browser, context) {
    const conn = browser._conn;
    const { targetId } = await conn.rootSend('Target.createTarget', {
      url: 'about:blank',
      browserContextId: context.id,
    });
    const session = await attachFlatSession(conn, targetId);
    const page = new Page(browser, context, targetId, session);
    await page._init();
    return page;
  }

  async _init() {
    const s = this._session;
    // Enable ONLY Page + Network here. Fetch is enabled lazily by authenticate()
    // (empty patterns) so it never clobbers the engine's own Fetch session.
    // Runtime/Console/Log/Debugger are NEVER enabled (anti-detection rule).
    await s.send('Page.enable');
    await s.send('Network.enable');
    await s.send('Page.setLifecycleEventsEnabled', { enabled: true }).catch(() => {});

    // Establish the main frame id + current url.
    try {
      const { frameTree } = await s.send('Page.getFrameTree');
      this._mainFrameId = frameTree.frame.id;
      this._mainFrame._url = frameTree.frame.url || 'about:blank';
    } catch { /* best effort */ }

    // Wire CDP events -> internal state + page-level events ('request', 'response', ...).
    s.on('Network.requestWillBeSent', (p) => this._onRequestWillBeSent(p));
    s.on('Network.responseReceived', (p) => this._onResponseReceived(p));
    s.on('Network.loadingFinished', (p) => this._settleBody(p.requestId));
    s.on('Network.loadingFailed', (p) => this._settleBody(p.requestId));
    s.on('Page.frameNavigated', (p) => this._onFrameNavigated(p));
    s.on('Page.lifecycleEvent', (p) => this._recordLifecycle(p));
    // These two are wired for API completeness but WILL NOT fire, because their
    // CDP source domain (Runtime) is intentionally never enabled. Diagnostics only.
    s.on('Runtime.consoleAPICalled', (p) => this._onConsole(p));
    s.on('Runtime.exceptionThrown', (p) => this._onException(p));
    // If the target detaches/crashes, mark the page closed.
    s.on('__disconnected__', () => { this._closed = true; });
    s.on('Inspector.targetCrashed', () => { this._closed = true; });
  }

  // ---- body-ready tracking (so response.text() can await loadingFinished) ----
  _bodyEntry(requestId) {
    let e = this._bodyReady.get(requestId);
    if (!e) {
      let resolve;
      const promise = new Promise((r) => { resolve = r; });
      e = { promise, resolve, done: false };
      this._bodyReady.set(requestId, e);
      // prune to avoid unbounded growth on long-lived pages
      if (this._bodyReady.size > 2000) {
        const it = this._bodyReady.keys();
        for (let i = 0; i < 500; i++) {
          const k = it.next().value;
          if (k === undefined) break;
          this._bodyReady.delete(k);
        }
      }
    }
    return e;
  }
  _settleBody(requestId) {
    const e = this._bodyReady.get(requestId);
    if (e && !e.done) { e.done = true; e.resolve(); }
  }
  async _waitBodyReady(requestId, timeoutMs) {
    const e = this._bodyEntry(requestId);
    if (e.done) return;
    await Promise.race([e.promise, new Promise((r) => setTimeout(r, timeoutMs).unref?.())]);
  }

  // ---- CDP event handlers ----
  _onRequestWillBeSent(p) {
    this._bodyEntry(p.requestId); // pre-create so text() can await even a fast finish
    try { this.emit('request', new CDPRequest(p)); } catch {}
  }
  _onResponseReceived(p) {
    this._bodyEntry(p.requestId);
    try { this.emit('response', new CDPResponse(this, p)); } catch {}
  }
  _onFrameNavigated(p) {
    const f = p.frame || {};
    if (!f.parentId) {
      // main frame — update the SINGLETON frame object so `frame === page.mainFrame()`
      // reference-equality (relied on by callers) holds.
      this._mainFrame._url = f.url || this._mainFrame._url;
      if (f.id) this._mainFrameId = f.id;
      try { this.emit('framenavigated', this._mainFrame); } catch {}
    } else {
      try { this.emit('framenavigated', new Frame(f.url)); } catch {}
    }
  }
  _recordLifecycle(p) {
    if (p.frameId !== this._mainFrameId) return;
    let set = this._lifecycleSeen.get(p.loaderId);
    if (!set) {
      set = new Set();
      this._lifecycleSeen.set(p.loaderId, set);
      if (this._lifecycleSeen.size > 50) {
        const k = this._lifecycleSeen.keys().next().value;
        if (k !== undefined) this._lifecycleSeen.delete(k);
      }
    }
    set.add(p.name);
  }
  _onConsole(p) {
    // Never actually fires (Runtime not enabled). Kept for API completeness.
    const text = (p.args || []).map((a) => (a.value !== undefined ? String(a.value) : a.description || '')).join(' ');
    try {
      this.emit('console', { type: () => p.type || 'log', text: () => text });
    } catch {}
  }
  _onException(p) {
    // Never actually fires (Runtime not enabled). Kept for API completeness.
    const d = p.exceptionDetails || {};
    const err = new Error(d.exception?.description || d.text || 'page error');
    try { this.emit('pageerror', err); } catch {}
  }

  // ---- page API surface ----

  target() {
    // createCDPSession() attaches ANOTHER flat session
    // to THIS page's target — distinct from the page's main session, so the engine
    // can run its own Fetch domain (stub serving) without touching the auth Fetch.
    return {
      createCDPSession: async () => attachFlatSession(this._browser._conn, this._targetId),
    };
  }

  mainFrame() { return this._mainFrame; }
  url() { return this._mainFrame._url; }
  isClosed() { return this._closed; }
  browserContext() { return this._context; }

  async goto(url, { waitUntil = 'load', timeout = this._browser._protocolTimeout } = {}) {
    const want = lifecycleName(waitUntil);
    return new Promise((resolve, reject) => {
      let settled = false;
      let loaderId = null;
      const cleanup = () => {
        clearTimeout(timer);
        this._session.off('Page.lifecycleEvent', onLc);
      };
      const finish = (fn, arg) => {
        if (settled) return;
        settled = true;
        cleanup();
        fn(arg);
      };
      const check = (name, lId) => {
        if (settled) return;
        if (loaderId && lId && lId !== loaderId) return; // event from a different navigation
        if (name === want) finish(resolve, null); // resolves null; the engine ignores the return value
      };
      const onLc = (p) => {
        if (p.frameId === this._mainFrameId) check(p.name, p.loaderId);
      };
      const timer = setTimeout(
        () => finish(reject, new TimeoutError(`Navigation timeout of ${timeout} ms exceeded`)),
        timeout,
      );
      timer.unref?.();
      this._session.on('Page.lifecycleEvent', onLc);

      this._session.send('Page.navigate', { url }).then((nav) => {
        if (nav.errorText && nav.errorText !== 'net::ERR_ABORTED') {
          finish(reject, new Error(`net::${nav.errorText} at ${url}`));
          return;
        }
        loaderId = nav.loaderId || null;
        // The desired lifecycle may have already fired before navigate resolved.
        if (loaderId) {
          const seen = this._lifecycleSeen.get(loaderId);
          if (seen && seen.has(want)) check(want, loaderId);
        }
      }).catch((e) => finish(reject, e));
    });
  }

  async evaluate(fn, ...args) {
    // Runtime.evaluate works WITHOUT Runtime.enable — it runs in the session's
    // default execution context. This is the core anti-detection trick.
    const expression = evaluationString(fn, ...args);
    const { result, exceptionDetails } = await this._session.send('Runtime.evaluate', {
      expression,
      returnByValue: true,
      awaitPromise: true,
      userGesture: true,
    });
    if (exceptionDetails) {
      throw new Error(
        exceptionDetails.exception?.description ||
        exceptionDetails.text ||
        'Evaluation failed',
      );
    }
    return result ? result.value : undefined;
  }

  async waitForFunction(fn, { timeout = this._browser._protocolTimeout, polling = 100 } = {}) {
    const expression = typeof fn === 'function' ? `(${fn.toString()})()` : `(${fn})`;
    const interval = typeof polling === 'number' && polling > 0 ? polling : 100; // 'raf'/'mutation' -> poll
    const start = Date.now();
    return new Promise((resolve, reject) => {
      const tick = async () => {
        if (this._closed) return reject(new Error('waitForFunction: page closed'));
        let value;
        try {
          const { result, exceptionDetails } = await this._session.send('Runtime.evaluate', {
            expression,
            returnByValue: true,
            awaitPromise: true,
          });
          if (!exceptionDetails) value = result ? result.value : undefined;
        } catch { /* transient — keep polling until timeout */ }
        if (value) return resolve(value); // truthy => predicate satisfied
        if (Date.now() - start >= timeout) {
          return reject(new TimeoutError(`waitForFunction timeout ${timeout}ms exceeded`));
        }
        const t = setTimeout(tick, interval);
        t.unref?.();
      };
      tick();
    });
  }

  // Proxy authentication. See the FETCH COEXISTENCE block at the top of the file:
  // we enable Fetch on the MAIN session with patterns:[] (intercept nothing) +
  // handleAuthRequests:true. That yields Fetch.authRequired for proxy 407s without
  // pausing normal traffic, so it never fights the engine's separate stub-serving
  // Fetch session.
  async authenticate({ username, password } = {}) {
    this._credentials = username ? { username, password: password || '' } : null;
    const s = this._session;
    if (!this._authWired) {
      this._authWired = true;
      s.on('Fetch.authRequired', async (ev) => {
        // Answer EVERY challenge with ProvideCredentials (proxy auth is per-
        // connection, so it recurs many times — CancelAuth-after-first would break
        // proxying, a deliberate deviation from the brief's suggestion). continueWithAuth
        // is idempotent per unique requestId, so there is no same-request loop.
        const authChallengeResponse = this._credentials
          ? { response: 'ProvideCredentials', username: this._credentials.username, password: this._credentials.password }
          : { response: 'CancelAuth' };
        await s.send('Fetch.continueWithAuth', { requestId: ev.requestId, authChallengeResponse }).catch(() => {});
      });
      s.on('Fetch.requestPaused', async (ev) => {
        // patterns:['*'] pauses EVERY request on this session - immediately continue each. That
        // catch-all-plus-continue is the standard CDP idiom for auth-only interception. The stub-serve Fetch
        // on a SEPARATE flat session still wins the Document (Chrome routes each paused request to ONE session).
        await s.send('Fetch.continueRequest', { requestId: ev.requestId }).catch(() => {});
      });
    }
    // CRITICAL: Chrome REJECTS `patterns:[]` together with `handleAuthRequests:true` ("Can't specify empty
    // patterns with handleAuth set") - the throw was swallowed by the caller's .catch(), silently breaking
    // proxy 407 auth (authRequired never fired) -> 100% failure on user:pass proxies. Catch-all pattern fixes it.
    await s.send('Fetch.enable', { handleAuthRequests: true, patterns: [{ urlPattern: '*' }] });
  }

  async setUserAgent(ua) {
    // Network is enabled, so Network.setUserAgentOverride is the primary path;
    // fall back to the Emulation domain variant if that fails.
    try {
      await this._session.send('Network.setUserAgentOverride', { userAgent: String(ua) });
    } catch {
      await this._session.send('Emulation.setUserAgentOverride', { userAgent: String(ua) }).catch(() => {});
    }
  }

  async setExtraHTTPHeaders(headers) {
    await this._session.send('Network.setExtraHTTPHeaders', { headers: headers || {} });
  }

  async addScriptToEvaluateOnNewDocument(fn, ...args) {
    const source = typeof fn === 'function' ? evaluationString(fn, ...args) : String(fn);
    const r = await this._session.send('Page.addScriptToEvaluateOnNewDocument', { source });
    return r?.identifier;
  }

  async cookies(...urls) {
    let list = urls.filter(Boolean);
    if (!list.length) {
      const u = this.url();
      if (/^https?:/i.test(u)) list = [u];
    }
    const params = list.length ? { urls: list } : {};
    const { cookies } = await this._session.send('Network.getCookies', params);
    return cookies || [];
  }

  async close() {
    if (this._closed) return;
    this._closed = true;
    await this._browser._conn.rootSend('Target.closeTarget', { targetId: this._targetId }).catch(() => {});
    await this._session.detach().catch(() => {});
    try { this.emit('close'); } catch {}
  }
}

// ---------------------------------------------------------------------------
// BrowserContext
// ---------------------------------------------------------------------------
class BrowserContext {
  constructor(browser, id) {
    this._browser = browser;
    this.id = id; // browserContextId (exposed as context.id per the contract)
  }
  async newPage() {
    return Page._create(this._browser, this);
  }
  async close() {
    await this._browser._conn.rootSend('Target.disposeBrowserContext', { browserContextId: this.id }).catch(() => {});
  }
}

// ---------------------------------------------------------------------------
// Browser
// ---------------------------------------------------------------------------
class Browser extends EventEmitter {
  constructor(connection, child, wsUrl) {
    super();
    this._conn = connection;
    this._child = child;
    this._wsUrl = wsUrl;
    this._protocolTimeout = connection._protocolTimeout;
    this._closed = false;
    this._disconnected = false;
    this.setMaxListeners(0);

    // Register the browser-level (root) session for event routing.
    const root = new CDPSession(connection, '');
    connection._sessions.set('', root);
    this._root = root;

    const fireDisconnect = () => {
      if (this._disconnected) return;
      this._disconnected = true;
      try { this.emit('disconnected'); } catch {}
    };
    // ws close (via connection) or child exit both mean "browser gone".
    root.on('__disconnected__', fireDisconnect);
    if (child) child.once('exit', fireDisconnect);
  }

  // Exposed for the engine: its /health, /metrics, /diag read browser.connected.
  // (Without this getter it was undefined -> chromeConnected always reported false.)
  get connected() {
    return !this._closed && !this._disconnected;
  }

  process() {
    return this._child || null;
  }

  async version() {
    const v = await this._conn.rootSend('Browser.getVersion');
    return v.product;
  }

  async createBrowserContext({ proxyServer } = {}) {
    const params = {};
    if (proxyServer) params.proxyServer = proxyServer;
    const { browserContextId } = await this._conn.rootSend('Target.createBrowserContext', params);
    return new BrowserContext(this, browserContextId);
  }

  async close() {
    if (this._closed) return;
    this._closed = true;
    // Ask Chrome to close cleanly (best effort, short window), then hard-kill.
    try {
      await Promise.race([
        this._conn.rootSend('Browser.close'),
        new Promise((r) => setTimeout(r, 2000).unref?.()),
      ]);
    } catch { /* ignore */ }
    try { this._conn.dispose(); } catch {}
    killTree(this._child);
  }
}

// ---------------------------------------------------------------------------
// Process teardown — cross-platform best effort (imports limited to child_process).
// ---------------------------------------------------------------------------
function killTree(child) {
  if (!child || child.killed) return;
  const pid = child.pid;
  try {
    if (process.platform === 'win32' && pid) {
      // taskkill /T kills the whole Chrome process tree (renderers etc.).
      spawnSync('taskkill', ['/pid', String(pid), '/T', '/F'], { stdio: 'ignore' });
    } else {
      child.kill('SIGKILL');
    }
  } catch {
    try { child.kill('SIGKILL'); } catch {}
  }
}

// ---------------------------------------------------------------------------
// launch()
// ---------------------------------------------------------------------------
export async function launch({ executablePath, args = [], protocolTimeout = 180000 } = {}) {
  if (!executablePath) throw new Error('launch: executablePath is required');

  const finalArgs = args.slice();
  // Force a CDP endpoint on an ephemeral port; we discover the exact ws URL from
  // Chrome's stderr banner ("DevTools listening on ws://...").
  if (!finalArgs.some((a) => a.startsWith('--remote-debugging-port'))) {
    finalArgs.push('--remote-debugging-port=0');
  }
  // Ensure a user-data-dir (unique) if the caller didn't supply one. We can't use
  // node:fs here (import restriction), but Chrome creates the dir itself.
  if (!finalArgs.some((a) => a.startsWith('--user-data-dir'))) {
    const base = process.env.TEMP || process.env.TMP || '/tmp';
    const uniq = `${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
    finalArgs.push(`--user-data-dir=${base}/cf-cloak-cdp-${uniq}`);
  }

  const child = spawn(executablePath, finalArgs, {
    stdio: ['ignore', 'ignore', 'pipe'], // we only need stderr for the ws banner
  });

  let wsUrl;
  try {
    wsUrl = await waitForWsEndpoint(child, 30000);
  } catch (e) {
    killTree(child); // launch failed before CDP was usable - don't orphan the Chrome process
    throw e;
  }

  const ws = new WebSocket(wsUrl, {
    perMessageDeflate: false,
    maxPayload: 256 * 1024 * 1024, // CDP frames (e.g. base64 bodies) can be large
    followRedirects: true,
  });
  try {
    await new Promise((resolve, reject) => {
      const onOpen = () => { ws.off('error', onErr); resolve(); };
      const onErr = (e) => { ws.off('open', onOpen); reject(e); };
      ws.once('open', onOpen);
      ws.once('error', onErr);
    });
  } catch (e) {
    killTree(child); // ws handshake failed - same orphan rule
    throw e;
  }

  const connection = new Connection(ws, protocolTimeout);
  return new Browser(connection, child, wsUrl);
}

// Parse the "DevTools listening on ws://..." line from Chrome stderr.
// Fallback: if a fixed (non-zero) --remote-debugging-port is present, poll
// http://127.0.0.1:<port>/json/version for webSocketDebuggerUrl.
function waitForWsEndpoint(child, timeoutMs) {
  return new Promise((resolve, reject) => {
    let buf = '';
    let done = false;
    const finish = (fn, arg) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      child.stderr?.off('data', onData);
      // C1 fix: keep DRAINING stderr for the process lifetime. If we just detach the listener the pipe
      // reverts to paused, its buffer fills on Chrome's stderr chatter (GPU warns, renderer crashes...),
      // the ~64KB OS pipe fills, Chrome's write() blocks, and the whole browser stalls on a long run.
      child.stderr?.resume();
      fn(arg);
    };
    const onData = (chunk) => {
      buf += chunk.toString();
      const m = buf.match(/DevTools listening on (ws:\/\/[^\s]+)/);
      if (m) finish(resolve, m[1].trim());
    };
    const timer = setTimeout(() => finish(reject, new Error(`Timed out (${timeoutMs}ms) waiting for Chrome CDP ws endpoint`)), timeoutMs);
    timer.unref?.();
    child.stderr?.on('data', onData);
    child.once('exit', (code) => finish(reject, new Error(`Chrome exited (code ${code}) before CDP endpoint was ready`)));
    child.once('error', (err) => finish(reject, err));
  });
}

// Default export so `import driver from './driver.mjs'` and
// `driver.launch(...)` both work (the engine currently does the latter).
export default { launch };
