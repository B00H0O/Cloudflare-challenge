// lib.mjs — CloakBrowser discovery + proxy parsing for the cf-cloak solver.
// Proxy parsing is reused from the raw-CDP scaffold; findCloakBrowser locates the
// CloakBrowser (antidetect Chromium) binary the driver launches.

import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';

// Locate the CloakBrowser binary. Order: CLOAK_BIN / CHROME_BIN env, then the
// newest chromium-<ver> under ~/.cloakbrowser. Returns a path or null.
export function findCloakBrowser() {
  for (const name of ['CLOAK_BIN', 'CHROME_BIN']) {
    const v = process.env[name];
    if (v && fs.existsSync(v)) return v;
  }
  const root = path.join(os.homedir(), '.cloakbrowser');
  let entries;
  try {
    entries = fs.readdirSync(root).filter((d) => d.startsWith('chromium-'));
  } catch {
    return null;
  }
  // newest version first (numeric-aware compare on the dotted version suffix)
  entries.sort((a, b) => cmpVersion(b.slice('chromium-'.length), a.slice('chromium-'.length)));
  const exe = process.platform === 'win32' ? 'chrome.exe' : 'chrome';
  for (const dir of entries) {
    // win: <dir>/chrome.exe ; linux bundle layouts vary, so also probe one level down
    const candidates = [
      path.join(root, dir, exe),
      path.join(root, dir, 'chrome-win64', exe),
      path.join(root, dir, 'chrome-linux64', exe),
      path.join(root, dir, 'chrome-linux', exe),
    ];
    const hit = candidates.find((p) => fs.existsSync(p));
    if (hit) return hit;
  }
  return null;
}

function cmpVersion(a, b) {
  const pa = a.split('.').map((n) => parseInt(n, 10) || 0);
  const pb = b.split('.').map((n) => parseInt(n, 10) || 0);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pa[i] || 0) - (pb[i] || 0);
    if (d) return d;
  }
  return 0;
}

const _isPort = (s) => /^\d{1,5}$/.test(s) && Number(s) >= 1 && Number(s) <= 65535;
function _defaultPort(type) { return type === 'https' ? '443' : type === 'socks5' || type === 'socks4' ? '1080' : '80'; }

// Parse a proxy string into { type, host, port, user, pass }. Accepts
// scheme://[user:pass@]host:port and bare host:port[:user:pass] forms.
export function parseProxy(raw) {
  if (!raw?.trim()) return null;
  let t = raw.trim();
  let type = 'http';
  const sm = t.match(/^([a-z][a-z0-9+.-]*):\/\//i);
  if (sm) {
    const s = sm[1].toLowerCase();
    if (s.startsWith('socks5') || s === 'socks') type = 'socks5';
    else if (s.startsWith('socks4')) type = 'socks4';
    else if (s === 'https') type = 'https';
    else type = 'http';
    try {
      const u = new URL(t);
      if (u.hostname) {
        return {
          type,
          host: u.hostname,
          port: u.port || _defaultPort(type),
          user: u.username ? decodeURIComponent(u.username) : undefined,
          pass: u.password ? decodeURIComponent(u.password) : '',
        };
      }
    } catch { /* malformed - strip scheme and fall through */ }
    t = t.slice(sm[0].length);
  }
  if (t.includes('@')) {
    const at = t.lastIndexOf('@');
    const left = t.slice(0, at);
    const right = t.slice(at + 1);
    const rp = right.split(':');
    const lp = left.split(':');
    let addr, auth;
    if (rp.length >= 2 && _isPort(rp[1])) { addr = right; auth = left; }
    else if (lp.length >= 2 && _isPort(lp[1])) { addr = left; auth = right; }
    else { addr = right; auth = left; }
    const [host, port] = addr.split(':');
    const ci = auth.indexOf(':');
    const user = ci >= 0 ? auth.slice(0, ci) : auth;
    const pass = ci >= 0 ? auth.slice(ci + 1) : '';
    return { type, host, port: port || _defaultPort(type), user: user || undefined, pass };
  }
  const p = t.split(':');
  if (p.length === 2) return { type, host: p[0], port: _isPort(p[1]) ? p[1] : _defaultPort(type), user: undefined, pass: '' };
  if (p.length >= 4) {
    const port1 = _isPort(p[1]);
    const port3 = _isPort(p[3]);
    const looksHost = (s) => s.includes('.');
    if (port1 && port3) {
      if (looksHost(p[2]) && !looksHost(p[0])) return { type, host: p[2], port: p[3], user: p[0] || undefined, pass: p[1] };
      return { type, host: p[0], port: p[1], user: p[2] || undefined, pass: p.slice(3).join(':') };
    }
    if (port1) return { type, host: p[0], port: p[1], user: p[2] || undefined, pass: p.slice(3).join(':') };
    if (port3) return { type, host: p[2], port: p[3], user: p[0] || undefined, pass: p[1] };
    return { type, host: p[0], port: p[1], user: p[2] || undefined, pass: p.slice(3).join(':') };
  }
  if (p.length === 3) return { type, host: p[0], port: _isPort(p[1]) ? p[1] : _defaultPort(type), user: p[2] || undefined, pass: '' };
  return { type, host: p[0], port: _defaultPort(type), user: undefined, pass: '' };
}

export function proxyServerUrl(proxy) {
  if (!proxy?.host) return null;
  const type = proxy.type || 'http';
  return `${type}://${proxy.host}:${proxy.port}`;
}
