# CF Managed Solver

Cloudflare Managed Challenge solver in Rust. Raw CDP over a hand-rolled WebSocket client, zero
crates, ~600KB binary; each solve runs in its own isolated browser context.

Solves the "Just a moment..." interstitial and returns the `cf_clearance` cookie plus headers
for TLS replay. Not a general browser API.

For Turnstile sitekey tokens or IUAM, use
[cloudflare-solver](https://github.com/B00H0O/cloudflare-solver) instead.

## Managed challenge

```
$ curl -X POST http://localhost:408/managed \
    -H "Content-Type: application/json" \
    -d '{"url":"https://example.com"}'
{
  "success": true,
  "elapsed": "2.54s",
  "elapsed_ms": 2537,
  "headers": {"Cookie": "cf_clearance=...;", "User-Agent": "..."},
  "cf_clearance": "...",
  "status": "completed"
}
```

## Run

Docker:

```
$ docker compose up -d --build
$ curl http://localhost:408/health
{"status":"ok","dyno":"local","capacity":20,"available":20,"active":0}
```

Native, Linux. Needs Chrome/Chromium 131+ (auto-detected, or set CHROME_BIN) and Rust 1.80+ to
build. The default port 408 is privileged on Linux, so set PORT:

```
$ cargo build --release
$ PORT=8080 ./target/release/cf-managed
```

Native, Windows:

```
PS> .\launch.ps1
```

Default PORT is 408. On Linux either override it (as above) or grant the binary
CAP_NET_BIND_SERVICE; under Docker the mapping `408:408` works as-is.

## API

### POST /managed

| Field | Required | Description |
|-------|----------|-------------|
| url | yes | target page to solve |
| proxy | no | per-request proxy, any form listed under Proxy below |

Response fields:

- `elapsed` / `elapsed_ms` - wall time of the solve
- `headers` - ready-made replay headers (Cookie + User-Agent)
- `status` - always "completed" on HTTP 200
- `timings` - per-stage breakdown (setup/navigation/solve), present only when DEBUG is set
- errors: 400 bad url or proxy JSON, 429 pool busy for longer than QUEUE_TIMEOUT_MS,
  500 solve failed - all as `{"success":false,"message":"..."}`

### POST /v1

FlareSolverr shape. Compatible with `request.get` clients; does not return page HTML:
`solution.response` and `solution.url` are empty strings. Use /managed for full output.

```
$ curl -X POST http://localhost:408/v1 \
    -H "Content-Type: application/json" \
    -d '{"cmd":"request.get","url":"https://example.com","maxTimeout":60000}'
```

| Field | Required | Description |
|-------|----------|-------------|
| cmd | yes | only `request.get` is supported |
| url | yes | must start with http:// or https:// |
| maxTimeout | no | per-solve timeout in ms; absent or 0 falls back to the server default (timeOut, 90000). Clamped to 1000..300000 |
| proxy | no | same forms as /managed |

Response:

```
{
  "status": "ok",
  "message": "Challenge solved!",
  "start_timestamp": 1759940000000,
  "end_timestamp": 1759940002537,
  "version": "2.0.0",
  "solution": {
    "url": "",
    "status": 200,
    "cookies": [{"name":"cf_clearance","value":"...","domain":"","path":"/"}],
    "userAgent": "...",
    "headers": {},
    "response": ""
  }
}
```

Errors use the same `{"success":false,"message":"..."}` shape as /managed, not the
FlareSolverr error envelope.

### GET /health

`capacity` = BROWSERS x TABS; `active` = solves in flight; `available` = free slots.

### Proxy

Per-request, on both /managed and /v1. The solve runs in an isolated browser context, so the
proxy applies to that solve only.

String form:

```
-d '{"url":"https://example.com","proxy":"http://user:pass@host:8080"}'
```

Object form:

```
-d '{"url":"https://example.com","proxy":{"host":"host","port":8080,"protocol":"socks5","username":"u","password":"p"}}'
```

What the code supports:

- schemes: http, https, socks4, socks5 (bare `socks` = socks5; scheme omitted = http)
- object keys: host, port (number or string), protocol or scheme (default http),
  username, password
- string form also accepts `host:port:user:pass` and percent-encoded credentials;
  IPv6 hosts in brackets work (`[::1]:1080`)
- socks4 carries a username only (no password, per protocol)
- socks and https schemes are routed through a built-in local bridge that performs the upstream
  handshake itself, so SOCKS5 user/pass auth works (Chrome cannot do that natively)

## How it works

A pool of persistent Chrome processes (BROWSERS x TABS). Each solve gets a fresh isolated
browser context with its own cookie jar, navigates to the target page, clicks the Turnstile
widget the interstitial renders, polls for the `cf_clearance` cookie, then disposes the
context.

## Settings

Set via environment or a `.env` file (also `config.env`, `cf.env`; first file found wins, real
env vars take precedence).

| Var | Default | Description |
|-----|---------|-------------|
| PORT | 408 | HTTP port (privileged on Linux) |
| BROWSERS | 2 | Chrome processes, clamped 1-16 (~1 per 2-3 cores) |
| TABS | 10 | Isolated solve contexts per browser, clamped 1-50 |
| timeOut | 90000 | Per-solve timeout, ms |
| HEADLESS | true | Chrome headless mode |
| PREWARM_BROWSER | true | Start browsers at boot instead of on first solve |
| QUEUE_TIMEOUT_MS | 20000 | Max wait for a free slot before returning 429 |
| CHROME_BIN | auto | Chrome/Chromium path (CHROME_PATH is the fallback name) |
| DEBUG | unset | Verbose logging (any value) |
| DYNO | local | Instance label, echoed in /health and the X-Dyno header |

Rule of thumb: BROWSERS x TABS = your CPU core count.

## Scaling

All numbers owner-measured, Aug 2026.

| Rig | Load | Avg | Result |
|-----|------|-----|--------|
| 12-core | parallel | - | 250 solves/min |
| 8-core/64GB VPS | sequential | 2.5s | 100% pass rate (30/30) |
| 8-core/64GB VPS | 10 parallel | 6.1s | - |
| 8-core/64GB VPS | 20 parallel | 11.2s | 93 solves/min, 100% pass rate (5/5) |
| 4-core VPS | sequential | 3.5s | 100% pass rate (30/30) |
| same box, FlareSolverr | sequential | 13.9s | - |

Up to 5.6x faster than FlareSolverr on the same box (2.5s vs 13.9s sequential). Throughput
scales with cores and connection quality. **Verdict: single-digit seconds per solve on a
modest VPS, and parallel throughput scales with cores.**

`docker compose up --scale` cannot be combined with the fixed `408:408` host port mapping.
Build once, then add instances on their own host port:

```
$ docker compose up -d --build
$ docker run -d --name solver2 -p 409:408 --shm-size 1g cf-managed
$ docker run -d --name solver3 -p 410:408 --shm-size 1g cf-managed
```

Or drop the `ports:` mapping from docker-compose.yml and put instances behind a round-robin
load balancer.

## The ceiling

- Solves the managed-challenge interstitial only. Interactive challenges (captcha, checkbox)
  and JS-detection walls are out of scope.
- `cf_clearance` is bound to the solving context's IP and User-Agent. Replay both from the same
  IP or the cookie is worthless; the per-request proxy is the lever.
- Cloudflare's proof-of-work costs ~1 core per tab for ~2s. No flag reduces it - scale
  horizontally (more instances), not vertically.
- /v1 returns no page HTML; it is a cookie minter, not a scraper API.
- Do NOT expose the service publicly: anyone who can reach it can mint solves through your IPs.

## License

MIT. See LICENSE.
