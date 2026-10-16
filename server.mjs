// server.mjs — cf-cloak: a Cloudflare managed-challenge clearance minter.
//
// Drives headless CloakBrowser (antidetect Chromium) over raw CDP via ./driver.mjs
// and returns a replayable cf_clearance cookie + the cookie set + User-Agent for
// any Cloudflare managed-challenge URL. No puppeteer, no Playwright.
//
// Why this works where a plain-Chrome raw-CDP solver does not: CloakBrowser hides
// the automation tells at the binary level, so Cloudflare issues a CLEAN clearance
// (no cf_chl_rc_ni retry cookie) that replays. The driver never enables Runtime,
// and this solver stays minimal — navigate and wait, no clicking and no
// stealth-override. It DOES drop non-essential resources (images/fonts/media/CSS)
// via Fetch (measured clean on CloakBrowser; trims time); BLOCK_RESOURCES=false disables.
//
// Usage:
//   node server.mjs                 # starts on PORT (default 3010), headless
//   curl -X POST http://127.0.0.1:3010/managed \
//        -H 'content-type: application/json' \
//        -d '{"url":"https://target","proxy":"http://user:pass@host:port"}'
//   -> { ok, cf_clearance, cookies, cookieHeader, userAgent, domain, elapsedMs }
//
// Replay the returned cookieHeader + userAgent through a Chrome-TLS client
// (curl_cffi impersonate="chrome", cycletls, tls-client) from the same exit/IP.

import http from 'node:http';
import driver from './driver.mjs';
import { findCloakBrowser, parseProxy, proxyServerUrl } from './lib.mjs';

const PORT = Number(process.env.PORT) || 3010;
const HOST = process.env.HOST || '127.0.0.1';
const MAX_CONCURRENT = Math.max(1, Number(process.env.MAX_CONCURRENT) || 2);
const SOLVE_TIMEOUT_MS = Math.max(10_000, Number(process.env.timeOut) || 60_000);
const NAV_TIMEOUT_MS = 20_000;
const HEADLESS = process.env.HEADLESS !== 'false'; // headless by default
const POLL_MS = Math.max(250, Number(process.env.POLL_MS) || 500); // cookie poll interval during a solve
const BLOCK_RESOURCES = process.env.BLOCK_RESOURCES !== 'false'; // drop images/fonts/media/CSS via Fetch (measured clean on CloakBrowser; trims solve time + bandwidth)

const CLOAK = findCloakBrowser();

let browser = null;
let _bootPromise = null;
let shuttingDown = false;

const stats = { startedAt: Date.now(), total: 0, active: 0, passed: 0, failed: 0, busy: 0 };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
function log(tag, msg) { console.log(`[${new Date().toISOString()}] [${tag}] ${msg}`); }

// ---- browser lifecycle (lazy, memoized, relaunch on disconnect) -------------
function launchArgs() {
  // Minimal on purpose. CloakBrowser applies its own patches; piling on
  // --disable-* / stealth flags only adds a detectable fingerprint. The driver
  // adds --remote-debugging-port and a unique --user-data-dir itself.
  const args = ['--no-first-run', '--no-default-browser-check'];
  if (HEADLESS) args.push('--headless=new');
  return args;
}

async function bootBrowser() {
  if (!CLOAK) throw new Error('CloakBrowser not found - install it or set CLOAK_BIN to its chrome(.exe)');
  log('BROWSER', `launching CloakBrowser (${HEADLESS ? 'headless' : 'headed'})...`);
  browser = await driver.launch({ executablePath: CLOAK, args: launchArgs() });
  browser.on('disconnected', () => {
    if (shuttingDown) return;
    log('BROWSER', 'disconnected - will relaunch on next request');
    browser = null; _bootPromise = null;
  });
  log('BROWSER', `up (pid=${browser.process()?.pid || '?'})`);
}

function ensureBrowser() {
  if (browser && browser.connected !== false) return Promise.resolve();
  if (!_bootPromise) {
    _bootPromise = bootBrowser().catch((e) => { _bootPromise = null; throw e; });
  }
  return _bootPromise;
}

// ---- the solve: passive navigate + poll for cf_clearance --------------------
async function solve(url, proxy) {
  const ctxOpts = proxy ? { proxyServer: proxyServerUrl(proxy) } : {};
  const context = await browser.createBrowserContext(ctxOpts);
  let page, fetchCdp = null;
  try {
    page = await context.newPage();
    if (proxy?.user) await page.authenticate({ username: proxy.user, password: proxy.pass || '' }).catch(() => {});

    if (BLOCK_RESOURCES) {
      // Drop non-essential resource types to trim load time + bandwidth. On a
      // separate flat session so it never fights proxy-auth Fetch. Measured clean
      // on CloakBrowser (cf_clearance still replays); scoped to these 4 types only.
      fetchCdp = await page.target().createCDPSession();
      fetchCdp.on('Fetch.requestPaused', (ev) => {
        fetchCdp.send('Fetch.failRequest', { requestId: ev.requestId, errorReason: 'Failed' }).catch(() => {});
      });
      await fetchCdp.send('Fetch.enable', { patterns: [
        { urlPattern: '*', resourceType: 'Image' },
        { urlPattern: '*', resourceType: 'Font' },
        { urlPattern: '*', resourceType: 'Media' },
        { urlPattern: '*', resourceType: 'Stylesheet' },
      ], handleAuthRequests: false }).catch(() => {});
    }

    // domcontentloaded, not 'load': the challenge interstitial may never fully
    // settle. A nav error is non-fatal - the challenge still runs and we poll.
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: NAV_TIMEOUT_MS }).catch(() => {});

    const deadline = Date.now() + SOLVE_TIMEOUT_MS;
    let cf = null, cookies = [];
    while (Date.now() < deadline) {
      await sleep(POLL_MS);
      cookies = await page.cookies(url).catch(() => []);
      cf = cookies.find((c) => c.name === 'cf_clearance');
      if (cf && cf.value) {
        // settle: let the post-clearance reload commit, then re-read the final set
        await sleep(500);
        const after = await page.cookies(url).catch(() => cookies);
        if (after.length) cookies = after;
        cf = cookies.find((c) => c.name === 'cf_clearance') || cf;
        break;
      }
    }
    if (!cf || !cf.value) throw new Error('cf_clearance not obtained within timeout (is this URL a Cloudflare managed challenge?)');

    const userAgent = await page.evaluate(() => navigator.userAgent).catch(() => '');
    const cookieHeader = cookies.map((c) => `${c.name}=${c.value}`).join('; ');
    const domain = cf.domain || '';
    return {
      cf_clearance: cf.value,
      domain,
      userAgent,
      cookieHeader,
      cookies: cookies.map((c) => ({ name: c.name, value: c.value, domain: c.domain, path: c.path })),
    };
  } finally {
    try { if (fetchCdp) await fetchCdp.detach(); } catch { /* discarding */ }
    try { if (page && !page.isClosed()) await page.close(); await context.close(); } catch { /* discarding */ }
  }
}

// ---- http helpers -----------------------------------------------------------
function send(res, status, body) {
  if (res.headersSent || res.writableEnded) return;
  const isJson = typeof body !== 'string';
  res.writeHead(status, { 'content-type': isJson ? 'application/json' : 'text/plain' });
  res.end(isJson ? JSON.stringify(body) : body);
}

async function readBody(req, limit = 16 * 1024) {
  let n = 0; const chunks = [];
  const to = setTimeout(() => { try { req.destroy(new Error('body timeout')); } catch {} }, 15_000);
  try {
    for await (const chunk of req) { n += chunk.length; if (n > limit) throw new Error('body too large'); chunks.push(chunk); }
  } finally { clearTimeout(to); }
  return Buffer.concat(chunks).toString('utf8');
}

function sanitizeError(raw) {
  let s = String(raw || 'Error').trim();
  if (/err_tunnel|err_proxy|tunnel/i.test(s)) return 'proxy connection failed';
  if (/err_connection_reset/i.test(s)) return 'connection reset';
  if (/err_(connection_)?timed_out|timeout/i.test(s)) return 'timeout';
  if (/err_name_not_resolved|no such host/i.test(s)) return 'DNS resolution failed';
  if (/target closed|browser has been closed|protocol error/i.test(s)) return 'browser closed';
  s = s.replace(/[A-Z]:\\[^\s'"]+/gi, '<path>').replace(/\/(?:home|tmp|var|mnt)\/[^\s'"]+/g, '<path>');
  s = s.replace(/\s+/g, ' ').trim();
  return s.length > 200 ? s.slice(0, 197) + '...' : s || 'Error';
}

// ---- handlers ---------------------------------------------------------------
async function handleManaged(req, res) {
  if (shuttingDown) return send(res, 503, { ok: false, error: 'shutting down' });
  if (stats.active >= MAX_CONCURRENT) { stats.busy++; res.setHeader('retry-after', '5'); return send(res, 429, { ok: false, error: `busy (${stats.active}/${MAX_CONCURRENT})` }); }

  let body;
  try { const raw = await readBody(req); body = raw ? JSON.parse(raw) : {}; }
  catch (e) { return send(res, 400, { ok: false, error: `invalid JSON body: ${String(e?.message || e).slice(0, 120)}` }); }

  const url = typeof body.url === 'string' ? body.url.trim() : '';
  if (!/^https?:\/\//i.test(url)) return send(res, 400, { ok: false, error: 'url is required and must start with http:// or https://' });

  let proxy = null;
  if (body.proxy) {
    proxy = parseProxy(String(body.proxy));
    if (!proxy?.host) return send(res, 400, { ok: false, error: 'invalid proxy - expected http://[user:pass@]host:port (socks5/https and bare host:port forms also accepted)' });
  }

  try { await ensureBrowser(); }
  catch (e) { return send(res, 503, { ok: false, error: `browser failed to launch: ${sanitizeError(e?.message || e)}` }); }

  if (stats.active >= MAX_CONCURRENT) { stats.busy++; return send(res, 429, { ok: false, error: `busy (${stats.active}/${MAX_CONCURRENT})` }); }
  stats.total++; stats.active++;
  const reqId = stats.total; const t0 = Date.now();
  log('SOLVE', `#${reqId} ${url} proxy=${proxy ? proxy.host : 'none'}`);

  try {
    const out = await solve(url, proxy);
    const elapsedMs = Date.now() - t0;
    stats.passed++;
    log('SOLVE', `#${reqId} OK cf_clearance=${out.cf_clearance.slice(0, 12)}... (${elapsedMs}ms)`);
    return send(res, 200, { ok: true, ...out, elapsedMs });
  } catch (e) {
    const elapsedMs = Date.now() - t0;
    stats.failed++;
    log('SOLVE', `#${reqId} FAIL (${elapsedMs}ms): ${e?.message || e}`);
    const timedOut = /timeout|not obtained/i.test(String(e?.message || e));
    return send(res, timedOut ? 504 : 502, { ok: false, error: sanitizeError(e?.message || e), elapsedMs });
  } finally {
    stats.active--;
  }
}

function handleHealth(req, res) {
  send(res, 200, {
    ok: !shuttingDown,
    uptime: Math.round((Date.now() - stats.startedAt) / 1000),
    browserUp: !!browser?.connected,
    headless: HEADLESS,
    cloakBrowser: CLOAK ? 'found' : 'NOT FOUND', // never expose the absolute binary path over HTTP
    stats: { total: stats.total, passed: stats.passed, failed: stats.failed, busy: stats.busy, active: stats.active, maxConcurrent: MAX_CONCURRENT },
  });
}

const server = http.createServer(async (req, res) => {
  res.setHeader('access-control-allow-origin', '*');
  res.setHeader('access-control-allow-headers', 'content-type');
  if (req.method === 'OPTIONS') return send(res, 204, '');
  try {
    if (req.method === 'GET' && req.url === '/health') return handleHealth(req, res);
    if (req.method === 'POST' && req.url === '/managed') return handleManaged(req, res);
    if (req.method === 'GET' && (req.url === '/' || req.url.startsWith('/?'))) {
      return send(res, 200, { name: 'cf-cloak', description: 'Cloudflare managed-challenge clearance minter (headless CloakBrowser over raw CDP)', endpoints: ['GET /health', 'POST /managed'] });
    }
    return send(res, 404, { ok: false, error: 'not found', valid: ['GET /health', 'POST /managed'] });
  } catch (e) { send(res, 500, { ok: false, error: sanitizeError(e?.message || e) }); }
});

server.on('error', (err) => { log('ERR', `server.listen: ${err?.code || ''} ${err?.message || err}`); process.exit(1); });
server.listen(PORT, HOST, () => {
  log('BOOT', `cf-cloak listening on http://${HOST}:${PORT} (max concurrent ${MAX_CONCURRENT}, ${HEADLESS ? 'headless' : 'headed'})`);
  log('BOOT', `cloakbrowser: ${CLOAK || 'NOT FOUND - set CLOAK_BIN'}`);
});

async function shutdown(sig) {
  if (shuttingDown) return;
  shuttingDown = true;
  log('STOP', `received ${sig}, closing browser...`);
  try { if (browser) await browser.close(); } catch {}
  server.close(() => process.exit(0));
  setTimeout(() => process.exit(1), 5000).unref?.();
}
process.on('SIGINT', () => shutdown('SIGINT'));
process.on('SIGTERM', () => shutdown('SIGTERM'));
process.on('unhandledRejection', (r) => log('ERR', `unhandledRejection: ${r?.message || r}`));
process.on('uncaughtException', (e) => log('ERR', `uncaughtException: ${e?.message || e}`));
