use crate::browser::{ChromeWorker, DevTools};
use crate::server::json;
use crate::proxy::ProxyConfig;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct SolveJob {
    pub url: String,
    pub proxy: Option<ProxyConfig>,
    pub timeout_override: Option<Duration>,
}

impl SolveJob {
    pub fn from_json(body: &str) -> Result<Self, String> {
        let url = match json::find_string(body, "url") {
            Some(v) if v.starts_with("http://") || v.starts_with("https://") => v,
            Some(_) => return Err("url must start with http:// or https://".into()),
            None => return Err("missing url parameter".into()),
        };
        Ok(Self { url, proxy: crate::proxy::parse_from_request_body(body)?, timeout_override: None })
    }
}

pub struct SolveOutcome {
    pub success: bool,
    pub cf_clearance: Option<String>,
    pub cookies: Option<Vec<(String, String)>>,
    pub user_agent: Option<String>,
    pub elapsed: String,
    pub elapsed_ms: u128,
    pub timings: Option<String>,
}

impl SolveOutcome {
    pub fn to_json(&self) -> String {
        let mut fields = vec![
            ("success", if self.success { "true" } else { "false" }.into()),
            ("elapsed", json::string(&self.elapsed)),
            ("elapsed_ms", self.elapsed_ms.to_string()),
        ];
        if let Some(ref cookies) = self.cookies {
            let mut headers = vec![("Cookie", json::string(&cookie_header_value(cookies)))];
            if let Some(ref ua) = self.user_agent { headers.push(("User-Agent", json::string(ua))); }
            fields.push(("headers", json::object(&headers)));
        } else if let Some(ref cf) = self.cf_clearance {
            let mut headers = vec![("Cookie", json::string(&format!("cf_clearance={};", cf)))];
            if let Some(ref ua) = self.user_agent { headers.push(("User-Agent", json::string(ua))); }
            fields.push(("headers", json::object(&headers)));
        }
        if let Some(ref cf) = self.cf_clearance { fields.push(("cf_clearance", json::string(cf))); }
        if std::env::var("DEBUG").is_ok() {
            if let Some(ref t) = self.timings { fields.push(("timings", t.clone())); }
        }
        fields.push(("status", json::string("completed")));
        json::object(&fields)
    }

    pub fn to_flaresolverr_json(&self, start_ts: u128) -> String {
        let end_ts = start_ts + self.elapsed_ms;
        let cookies_json = if let Some(ref cookies) = self.cookies {
            format!("[{}]", cookies.iter()
                .map(|(n, v)| format!("{{\"name\":{},\"value\":{},\"domain\":\"\",\"path\":\"/\"}}", json::string(n), json::string(v)))
                .collect::<Vec<_>>().join(","))
        } else { "[]".into() };
        let ua = json::string(self.user_agent.as_deref().unwrap_or(""));
        let solution = format!("{{\"url\":{},\"status\":200,\"cookies\":{},\"userAgent\":{},\"headers\":{{}},\"response\":\"\"}}", json::string(""), cookies_json, ua);
        json::object(&[
            ("status", json::string("ok")),
            ("message", json::string("Challenge solved!")),
            ("start_timestamp", start_ts.to_string()),
            ("end_timestamp", end_ts.to_string()),
            ("version", json::string("2.0.0")),
            ("solution", solution),
        ])
    }
}

fn cookie_header_value(cookies: &[(String, String)]) -> String {
    let mut h = cookies.iter().map(|(n, v)| format!("{}={}", n, v)).collect::<Vec<_>>().join("; ");
    if !h.is_empty() { h.push(';'); }
    h
}

const POLL_INTERVAL_MS: u64 = 75;
const CLICK_INTERVAL_MS: u64 = 30;
const CLICK_ACTION_TIMEOUT_MS: u64 = 250;
const CLICK_MAX_ATTEMPTS: u32 = 240;

const STEALTH_SCRIPT: &str = "(function(){Object.defineProperty(navigator,'webdriver',{get:function(){return undefined;},configurable:true});if(!window.chrome){Object.defineProperty(window,'chrome',{value:{runtime:{}},configurable:true});}var origQuery=window.navigator.permissions&&window.navigator.permissions.query;if(origQuery){window.navigator.permissions.query=function(parameters){if(parameters.name==='notifications'){return Promise.resolve({state:Notification.permission});}return origQuery.call(window.navigator.permissions,parameters);}}})();";

pub struct SolverPool {
    browsers: Vec<WorkerEntry>,
    tabs_each: usize,
    timeout: Duration,
    headless: bool,
    next: AtomicUsize,
    _browser_max_age: Duration,
    _browser_max_solves: u64,
}

struct WorkerEntry {
    manager: Mutex<ChromeWorker>,
    available: AtomicUsize,
    created_at: Instant,
    solve_count: AtomicU64,
}

pub enum SolveError { TooManyRequests, Internal(String) }

impl SolverPool {
    pub fn new(timeout: Duration, browsers_count: usize, tabs_each: usize, headless: bool) -> Self {
        let mut browsers = Vec::with_capacity(browsers_count);
        for _ in 0..browsers_count {
            browsers.push(WorkerEntry {
                manager: Mutex::new(ChromeWorker::new(timeout)),
                available: AtomicUsize::new(tabs_each),
                created_at: Instant::now(),
                solve_count: AtomicU64::new(0),
            });
        }
        Self {
            browsers, tabs_each, timeout, headless,
            next: AtomicUsize::new(0),
            _browser_max_age: Duration::from_secs(1800),
            _browser_max_solves: 50,
        }
    }

    pub fn timeout(&self) -> Duration { self.timeout }

    pub fn capacity_snapshot(&self) -> (usize, usize, usize) {
        let cap = self.browsers.len() * self.tabs_each;
        let avail = self.browsers.iter().map(|e| e.available.load(Ordering::SeqCst)).sum::<usize>().min(cap);
        (cap, avail, cap.saturating_sub(avail))
    }

    pub fn solve(&self, request: SolveJob) -> Result<SolveOutcome, SolveError> {
        let started_at = Instant::now();
        let timeout = request.timeout_override.unwrap_or(self.timeout);
        let slot = self.acquire_tab()?;
        let entry = &self.browsers[slot.browser_index];

        let (context_page, product) = {
            let mut mgr = entry.manager.lock().map_err(|e| SolveError::Internal(e.to_string()))?;
            mgr.ensure_started(self.headless, None).map_err(SolveError::Internal)?;
            let product = mgr.browser_product_cached();
            let page = mgr.create_context_page(request.proxy.as_ref()).map_err(SolveError::Internal)?;
            (page, product)
        };

        let result = (|| -> Result<SolveOutcome, String> {
            let mut cdp = DevTools::connect(&context_page.ws_url, timeout, self.headless)?;
            run_solve_on_cdp(&mut cdp, &request, product.as_deref(), timeout, self.headless, started_at)
        })();

        if let Ok(mut mgr) = entry.manager.lock() { mgr.dispose_context_page(&context_page); }
        result.map_err(SolveError::Internal)
    }

    fn acquire_tab(&self) -> Result<PoolSlot<'_>, SolveError> {
        let count = self.browsers.len();
        if count == 0 { return Err(SolveError::TooManyRequests); }
        let deadline = Instant::now() + queue_timeout();
        loop {
            if crate::shutdown::is_requested() { return Err(SolveError::TooManyRequests); }
            for _ in 0..count {
                let index = self.next.fetch_add(1, Ordering::SeqCst) % count;
                let avail = &self.browsers[index].available;
                let cur = avail.load(Ordering::SeqCst);
                if cur > 0 && avail.compare_exchange(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                    return Ok(PoolSlot { available: avail, browser_index: index });
                }
            }
            if Instant::now() >= deadline { return Err(SolveError::TooManyRequests); }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn prewarm(&self, headless: bool) -> Result<(), String> {
        for entry in &self.browsers {
            let mut mgr = entry.manager.lock().map_err(|e| format!("lock: {}", e))?;
            mgr.ensure_started(headless, None)?;
            mgr.ensure_browser_cdp()?;
            let _ = mgr.browser_product_cached();
        }
        Ok(())
    }

    pub fn shutdown(&self) {
        for entry in &self.browsers {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match entry.manager.try_lock() {
                    Ok(mut mgr) => { mgr.shutdown(); break; }
                    Err(std::sync::TryLockError::Poisoned(e)) => { e.into_inner().shutdown(); break; }
                    Err(std::sync::TryLockError::WouldBlock) if Instant::now() < deadline => { std::thread::sleep(Duration::from_millis(25)); }
                    Err(std::sync::TryLockError::WouldBlock) => break,
                }
            }
        }
    }
}

struct PoolSlot<'a> { available: &'a AtomicUsize, browser_index: usize }
impl Drop for PoolSlot<'_> {
    fn drop(&mut self) { self.available.fetch_add(1, Ordering::SeqCst); }
}

fn queue_timeout() -> Duration {
    Duration::from_millis(std::env::var("QUEUE_TIMEOUT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000))
}

pub fn run_solve_on_cdp(
    cdp: &mut DevTools, request: &SolveJob, _product: Option<&str>,
    timeout: Duration, headless: bool, started_at: Instant,
) -> Result<SolveOutcome, String> {
    let setup_start = Instant::now();
    cdp.headless = headless;
    cdp.attempted_authentications.clear();

    let stealth = format!("{{\"source\":{}}}", json::string(STEALTH_SCRIPT));
    let blocked = r#"{"urls":["*google-analytics.com*","*googletagmanager.com*","*doubleclick.net*","*connect.facebook.net*","*platform.twitter.com*","*static.hotjar.com*","*cdn.segment.com*","*www.googletagservices.com*","*adservice.google.com*","*ads.google.com*","*googleads.g.doubleclick.net*","*adnxs.com*","*criteo.com*","*taboola.com*","*outbrain.com*","*scorecardresearch.com*","*quantserve.com*","*moatads.com*"]}"#;
    let fetch_enable = r#"{"patterns":[{"urlPattern":"*","resourceType":"Image"},{"urlPattern":"*","resourceType":"Font"},{"urlPattern":"*","resourceType":"Media"},{"urlPattern":"*","resourceType":"Stylesheet"}],"handleAuthRequests":false}"#;
    cdp.call_burst(vec![
        ("Page.enable", "{}"),
        ("Network.enable", "{}"),
        ("Network.setBlockedURLs", blocked),
        ("Fetch.enable", fetch_enable),
        ("DOM.enable", "{}"),
        ("Page.addScriptToEvaluateOnNewDocument", &stealth),
    ], timeout)?;
    cdp.interception_enabled = true;

    let after_setup = Instant::now();
    let nav_start = Instant::now();
    cdp.call("Page.navigate", &format!("{{\"url\":{}}}", json::string(&request.url)), timeout)?;

    if cdp.wait_for_event("Page.domContentEventFired", Duration::from_secs(8)).is_err() {
        let _ = cdp.evaluate_string_with_timeout("window.stop()", Duration::from_millis(500));
        return Err("DOM did not load within 8s".into());
    }
    let after_nav = Instant::now();

    let timings = StageTimings {
        setup_ms: after_setup.duration_since(setup_start).as_millis(),
        navigation_ms: after_nav.duration_since(nav_start).as_millis(),
    };

    let solve_start = Instant::now();
    let result = solve_managed(cdp, &request.url, timeout, started_at, timings, solve_start);

    cdp.interception_enabled = false;
    let _ = cdp.call("Fetch.disable", "{}", Duration::from_secs(2));
    let _ = cdp.call("Page.navigate", "{\"url\":\"about:blank\"}", Duration::from_secs(2));
    result
}

#[derive(Clone)]
struct StageTimings { setup_ms: u128, navigation_ms: u128 }

impl StageTimings {
    fn to_json(&self, solve_ms: u128, total_ms: u128) -> String {
        json::object(&[
            ("setup_ms", self.setup_ms.to_string()),
            ("navigation_ms", self.navigation_ms.to_string()),
            ("solve_ms", solve_ms.to_string()),
            ("total_ms", total_ms.to_string()),
        ])
    }
}

fn solve_managed(cdp: &mut DevTools, url: &str, timeout: Duration, started_at: Instant, timings: StageTimings, solve_start: Instant) -> Result<SolveOutcome, String> {
    let deadline = Instant::now() + timeout;
    let click_interval = Duration::from_millis(CLICK_INTERVAL_MS);
    let click_timeout = Duration::from_millis(CLICK_ACTION_TIMEOUT_MS);
    let mut attempts = 0u32;
    let mut last_click = Instant::now().checked_sub(click_interval).unwrap_or_else(Instant::now);

    loop {
        if crate::shutdown::is_requested() { return Err("shutdown requested".into()); }

        let remaining = deadline.checked_duration_since(Instant::now())
            .ok_or_else(|| "timed out waiting for cf_clearance".to_string())?;
        let call_timeout = remaining.min(Duration::from_secs(5));

        match cdp.call("Network.getCookies", &format!("{{\"urls\":[{}]}}", json::string(url)), call_timeout) {
            Ok(resp) => {
                let cookies = find_cookie_values(&resp, &["cf_clearance", "__ts_bm"]);
                if cookies.iter().any(|(n, _)| n == "cf_clearance") {
                    let elapsed = started_at.elapsed();
                    let solve_ms = solve_start.elapsed().as_millis();
                    return Ok(build_outcome(cdp, url, elapsed, &timings, solve_ms));
                }
            }
            Err(e) if is_transient_nav_error(&e) => {}
            Err(e) => { if e.contains("timed out") { continue; } return Err(format!("read cookies: {}", e)); }
        }

        if attempts < CLICK_MAX_ATTEMPTS && last_click.elapsed() >= click_interval {
            attempts += 1;
            if std::env::var("DEBUG").is_ok() && attempts % 10 == 1 {
                println!("[Solver] click attempt #{} ({}ms)", attempts, solve_start.elapsed().as_millis());
            }
            match try_click(cdp, click_timeout) {
                Ok(()) => { last_click = Instant::now(); }
                Err(e) => { if std::env::var("DEBUG").is_ok() && attempts % 20 == 1 { println!("[Solver] click err: {}", e); } }
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() { return Err("timed out waiting for cf_clearance".into()); }
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS).min(remaining));
    }
}

fn try_click(cdp: &mut DevTools, timeout: Duration) -> Result<(), String> {
    if let Some((x, y)) = cdp.find_shadow_challenge_click_target(timeout)? { cdp.click(x, y, timeout)?; }
    Ok(())
}

fn build_outcome(cdp: &mut DevTools, url: &str, elapsed: Duration, timings: &StageTimings, solve_ms: u128) -> SolveOutcome {
    let cookies = match cdp.call("Network.getCookies", &format!("{{\"urls\":[{}]}}", json::string(url)), Duration::from_secs(3)) {
        Ok(resp) => find_cookie_values(&resp, &["cf_clearance", "__ts_bm"]),
        Err(_) => Vec::new(),
    };
    let clearance = cookies.iter().find_map(|(n, v)| if n == "cf_clearance" { Some(v.clone()) } else { None }).unwrap_or_default();
    let ua = cdp.evaluate_string_with_timeout("navigator.userAgent", Duration::from_secs(2)).unwrap_or_default();
    let elapsed_ms = elapsed.as_millis();
    SolveOutcome {
        success: !clearance.is_empty(),
        cf_clearance: Some(clearance),
        cookies: Some(cookies),
        user_agent: Some(ua),
        elapsed: format!("{:.2}s", elapsed.as_secs_f64()),
        elapsed_ms,
        timings: Some(timings.to_json(solve_ms, elapsed_ms)),
    }
}

fn find_cookie_values(resp: &str, names: &[&str]) -> Vec<(String, String)> {
    let Ok(val) = json::parse(resp) else { return Vec::new() };
    let Some(cookies) = val.get("result").and_then(|r| r.get("cookies")).and_then(|c| c.as_array()) else { return Vec::new() };
    let mut found = Vec::new();
    for name in names {
        for cookie in cookies {
            if cookie.get("name").and_then(|v| v.as_str()) == Some(*name) {
                if let Some(v) = cookie.get("value").and_then(val_to_string) { found.push(((*name).into(), v)); }
                break;
            }
        }
    }
    found
}

fn val_to_string(v: &json::Value) -> Option<String> {
    match v {
        json::Value::String(s) => Some(s.clone()),
        json::Value::Number(n) => Some(if n.fract() == 0.0 { format!("{:.0}", n) } else { n.to_string() }),
        json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn is_transient_nav_error(e: &str) -> bool {
    e.contains("Execution context was destroyed") || e.contains("Cannot find context") || e.contains("Inspected target navigated")
}
