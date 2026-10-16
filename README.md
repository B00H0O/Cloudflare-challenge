# CF Cloak

Pass the Cloudflare managed-challenge ("Just a moment...") with headless CloakBrowser over raw
CDP, and get back a `cf_clearance` cookie you can replay over plain HTTP. Node.js, one small
server, one dependency. 

For Turnstile tokens or IUAM, use
[cloudflare-solver](https://github.com/B00H0O/cloudflare-solver) instead.

## Solve

```
$ npm install
$ node server.mjs
$ curl -X POST http://localhost:3010/managed \
    -H "content-type: application/json" \
    -d '{"url":"https://the-site.com/"}'

{"ok":true,"cf_clearance":"...","domain":".the-site.com","userAgent":"Mozilla/5.0 ...","cookieHeader":"cf_clearance=...; ...","cookies":[...],"elapsedMs":5100}
```

## Run

Needs [CloakBrowser](https://cloakbrowser.dev) (an antidetect Chromium) and Node 20+. CloakBrowser
is auto-detected under `~/.cloakbrowser`, or point `CLOAK_BIN` at its `chrome` / `chrome.exe`.

```
npm install
node server.mjs        # http://127.0.0.1:3010
```

Runs headless on Linux or Windows. API is on `http://127.0.0.1:3010`.

## API

`GET /health` returns `{"ok","uptime","browserUp","headless","stats"}`.

### `POST /managed`

Load the real page, pass the challenge, return the `cf_clearance` cookie plus the cookie jar, the
User-Agent, and the cookie domain.

```json
{"url":"https://the-site.com/"}
```

| Field | Required | Description |
|-------|----------|-------------|
| url | yes | Target URL (http or https) |
| proxy | no | `http://user:pass@host:port`, `socks5://...`, `socks4://...`, or `host:port[:user:pass]` |

Response: `ok`, `cf_clearance`, `domain`, `userAgent`, `cookieHeader`, `cookies[]` (name/value/domain/path), `elapsedMs`.

`cf_clearance` is tied to the IP, the User-Agent, and the TLS fingerprint it was earned on. Replay
from the same IP (use the same proxy you solved with), send back the `userAgent`, and use an HTTP
client that speaks a browser TLS fingerprint — for example
[curl_cffi](https://github.com/lexiforest/curl_cffi):

```python
from curl_cffi import requests

s = requests.post("http://localhost:3010/managed",
                  json={"url": "https://the-site.com/"}).json()

r = requests.get("https://the-site.com/",
                 headers={"User-Agent": s["userAgent"]},
                 cookies=dict(p.split("=", 1) for p in s["cookieHeader"].split("; ")),
                 impersonate="chrome")
print(r.status_code)
```

## How it works

One CloakBrowser process stays running. For each solve, a fresh isolated browser context (its own
cookie jar, no cross-solve leakage) loads the target page, drops images/fonts/CSS to keep it light,
and polls for `cf_clearance`. Once it lands, the context is thrown away.

## Settings

All optional, defaults work out of the box.

| Variable | Default | Description |
|----------|---------|-------------|
| PORT | 3010 | HTTP port |
| HOST | 127.0.0.1 | bind address (`0.0.0.0` to expose) |
| MAX_CONCURRENT | 2 | parallel solves |
| timeOut | 60000 | ms before a solve is given up on |
| HEADLESS | true | set `false` to watch the browser |
| BLOCK_RESOURCES | true | drop images/fonts/media/CSS during the solve |
| CLOAK_BIN | auto | CloakBrowser path (else auto-detected under `~/.cloakbrowser`) |

## Scaling

Each solve waits on Cloudflare's interstitial (~5 s), so throughput comes from running solves in
parallel: raise `MAX_CONCURRENT` and rotate proxies. The challenge is CPU-heavy, so scale with
cores — one browser context per solve.

## License

MIT. See LICENSE.
