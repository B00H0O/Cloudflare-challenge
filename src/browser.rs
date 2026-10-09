use crate::server::json;
use crate::server::json::Value;
use crate::proxy::{LocalProxyBridge, ProxyConfig};
use crate::ws::WebSocket;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct ChromeWorker {
    pub(crate) process: Option<Child>,
    pub(crate) port: Option<u16>,
    pub(crate) user_data_dir: Option<PathBuf>,
    pub(crate) cdp: Option<DevTools>,
    pub(crate) browser_cdp: Option<DevTools>,
    pub(crate) browser_product: Option<String>,
    pub(crate) timeout: Duration,
    pub(crate) headless: bool,
    pub current_proxy: Option<ProxyConfig>,
    proxy_bridge: Option<LocalProxyBridge>,
}

pub(crate) struct TabHandle {
    pub(crate) browser_context_id: String,
    pub(crate) target_id: String,
    pub(crate) ws_url: String,
}

impl ChromeWorker {
    pub fn new(timeout: Duration) -> Self {
        Self {
            process: None,
            port: None,
            user_data_dir: None,
            cdp: None,
            browser_cdp: None,
            browser_product: None,
            timeout,
            headless: false,
            current_proxy: None,
            proxy_bridge: None,
        }
    }

    pub(crate) fn ensure_started(
        &mut self,
        headless: bool,
        proxy: Option<ProxyConfig>,
    ) -> Result<(), String> {
        let headless_changed = self.process.is_some() && self.headless != headless;
        let proxy_changed = self.current_proxy != proxy;
        if self.current_proxy != proxy {
            self.current_proxy = proxy;
        }
        self.headless = headless;

        if let Some(process) = self.process.as_mut() {
            if proxy_changed || headless_changed {
                if env::var("DEBUG").is_ok() {
                    println!("[Browser] Browser config changed. Restarting Chrome...");
                }
                let _ = process.kill();
                let _ = process.wait();
                self.process = None;
                self.port = None;
                self.cdp = None;
                self.browser_cdp = None;
                self.browser_product = None;
                self.proxy_bridge = None;
                if let Some(dir) = self.user_data_dir.take() {
                    let _ = fs::remove_dir_all(dir);
                }
            } else {
                match process.try_wait() {
                    Ok(None) => return Ok(()),
                    Ok(Some(status)) => {
                        eprintln!("[Browser] previous Chrome exited: {}", status);
                        self.process = None;
                        self.port = None;
                        self.cdp = None;
                        self.browser_cdp = None;
                        self.browser_product = None;
                        self.proxy_bridge = None;
                    }
                    Err(err) => {
                        eprintln!("[Browser] failed to inspect Chrome process: {}", err);
                        self.process = None;
                        self.port = None;
                        self.cdp = None;
                        self.browser_cdp = None;
                        self.browser_product = None;
                        self.proxy_bridge = None;
                    }
                }
            }
        }

        self.proxy_bridge = None;

        let chrome = find_chrome_executable()?;
        let port = free_port()?;
        let user_data_dir =
            env::temp_dir().join(format!("cfmanaged-{}-{}", std::process::id(), port));
        fs::create_dir_all(&user_data_dir)
            .map_err(|err| format!("failed to create user data dir: {}", err))?;

        // Shared disk cache across all browser instances — CF's challenge JS
        // (api.js, turnstile/v0/api.js) is cached after first solve and reused
        // on subsequent browser launches, cutting navigation time.
        let disk_cache_dir = env::temp_dir().join("cfmanaged-shared-cache");
        fs::create_dir_all(&disk_cache_dir)
            .map_err(|err| format!("failed to create disk cache dir: {}", err))?;

        let disabled_features = concat!(
            "Translate,",
            "BackForwardCache,",
            "AcceptCHFrame,",
            "MediaRouter,",
            "OptimizationHints,",
            "RenderDocument,",
            "PartitionAllocSchedulerLoopQuarantineTaskControlledPurge,",
            "ProcessPerSiteUpToMainFrameThreshold,",
            "IsolateSandboxedIframes,",
            "IsolateOrigins,",
            "site-per-process"
        );
        let enabled_features = "PdfOopif";

        let mut args = vec![
            "--allow-pre-commit-input".to_string(),
            "--disable-background-networking".to_string(),
            "--disable-background-timer-throttling".to_string(),
            "--disable-backgrounding-occluded-windows".to_string(),
            "--disable-breakpad".to_string(),
            "--disable-client-side-phishing-detection".to_string(),
            "--disable-component-extensions-with-background-pages".to_string(),
            "--disable-crash-reporter".to_string(),
            "--disable-default-apps".to_string(),
            "--disable-dev-shm-usage".to_string(),
            "--disable-hang-monitor".to_string(),
            "--disable-infobars".to_string(),
            "--disable-ipc-flooding-protection".to_string(),
            "--disable-popup-blocking".to_string(),
            "--disable-prompt-on-repost".to_string(),
            "--disable-renderer-backgrounding".to_string(),
            "--disable-renderer-accessibility".to_string(),
            "--disable-search-engine-choice-screen".to_string(),
            "--disable-sync".to_string(),
            "--force-color-profile=srgb".to_string(),
            "--no-default-browser-check".to_string(),
            "--no-first-run".to_string(),
            "--no-pings".to_string(),
            "--disable-domain-reliability".to_string(),
            "--password-store=basic".to_string(),
            "--use-mock-keychain".to_string(),
            format!("--disable-features={}", disabled_features),
            format!("--enable-features={}", enabled_features),
        ];

        // headless mode
        if headless {
            args.push("--headless=new".to_string());
        }
        args.push("--hide-scrollbars".to_string());
        args.push("--mute-audio".to_string());

        args.push("--disable-extensions".to_string());
        args.push("--disable-blink-features=AutomationControlled".to_string());
        args.push("about:blank".to_string());

        args.extend([
            "--disable-site-isolation-trials".to_string(),
            "--disable-gpu".to_string(),
            "--no-sandbox".to_string(),
            "--disable-setuid-sandbox".to_string(),
            "--disable-notifications".to_string(),
            // CPU overhead reduction — safe flags that don't touch JS execution
            "--disable-threaded-animation".to_string(),
            "--disable-threaded-scrolling".to_string(),
            "--disable-checker-imaging".to_string(),
            "--disable-composited-antialiasing".to_string(),
        ]);

        let mut proxy_bridge = None;
        if let Some(ref proxy) = self.current_proxy {
            let proxy_server = if proxy.requires_bridge() {
                let bridge = LocalProxyBridge::start(proxy.clone(), self.timeout)?;
                let server = bridge.chrome_proxy_server();
                proxy_bridge = Some(bridge);
                server
            } else {
                proxy.chrome_proxy_server()
            };
            args.push(format!("--proxy-server={}", proxy_server));
        }

        args.push(format!("--remote-debugging-port={}", port));
        args.push(format!("--user-data-dir={}", user_data_dir.display()));
        args.push(format!("--disk-cache-dir={}", disk_cache_dir.display()));

        if std::env::var("DEBUG").is_ok() {
            println!("[Browser] Launching {}", chrome.display());
        }
        let mut child = Command::new(&chrome)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| format!("failed to launch Chrome '{}': {}", chrome.display(), err))?;

        match wait_for_version(port, self.timeout) {
            Ok(()) => {}
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_dir_all(&user_data_dir);
                return Err(err);
            }
        };
        self.proxy_bridge = proxy_bridge;
        self.process = Some(child);
        self.port = Some(port);
        self.user_data_dir = Some(user_data_dir);
        self.cdp = None;
        self.browser_cdp = None;
        self.browser_product = None;
        Ok(())
    }

    pub(crate) fn shutdown(&mut self) {
        if let Some(process) = self.process.as_mut() {
            let _ = process.kill();
            let _ = process.wait();
        }
        self.process = None;
        self.port = None;
        self.cdp = None;
        self.browser_cdp = None;
        self.browser_product = None;
        self.proxy_bridge = None;
        self.current_proxy = None;

        if let Some(dir) = self.user_data_dir.take() {
            let _ = fs::remove_dir_all(dir);
        }
    }

    pub(crate) fn browser_product_cached(&mut self) -> Option<String> {
        if self.browser_product.is_none() {
            if let Some(port) = self.port {
                self.browser_product = browser_product(port, self.timeout).ok();
            }
        }
        self.browser_product.clone()
    }

    pub(crate) fn ensure_browser_cdp(&mut self) -> Result<(), String> {
        if self.browser_cdp.is_some() {
            return Ok(());
        }
        let port = self
            .port
            .ok_or_else(|| "browser not started (no port)".to_string())?;
        let ws = browser_ws_url(port, self.timeout)?;
        self.browser_cdp = Some(DevTools::connect(&ws, self.timeout, self.headless)?);
        Ok(())
    }

    pub(crate) fn create_context_page(
        &mut self,
        proxy: Option<&ProxyConfig>,
    ) -> Result<TabHandle, String> {
        self.ensure_browser_cdp()?;
        let port = self
            .port
            .ok_or_else(|| "browser not started (no port)".to_string())?;
        let timeout = self.timeout;
        let cdp = self
            .browser_cdp
            .as_mut()
            .ok_or_else(|| "browser CDP not available".to_string())?;

        let ctx_params = match proxy {
            Some(proxy) => format!(
                "{{\"proxyServer\":{}}}",
                json::string(&proxy.chrome_proxy_server())
            ),
            None => "{}".to_string(),
        };
        let resp = cdp.call("Target.createBrowserContext", &ctx_params, timeout)?;
        let browser_context_id = json::find_string(&resp, "result.browserContextId")
            .ok_or_else(|| format!("createBrowserContext returned no id: {}", resp))?;

        let target_params = format!(
            "{{\"url\":\"about:blank\",\"browserContextId\":{}}}",
            json::string(&browser_context_id)
        );
        let resp = cdp.call("Target.createTarget", &target_params, timeout)?;
        let target_id = json::find_string(&resp, "result.targetId")
            .ok_or_else(|| format!("createTarget returned no targetId: {}", resp))?;

        let ws_url = format!("ws://127.0.0.1:{}/devtools/page/{}", port, target_id);
        Ok(TabHandle {
            browser_context_id,
            target_id,
            ws_url,
        })
    }

    pub(crate) fn is_healthy(&self) -> bool {
        self.process.is_some() && self.port.is_some()
    }

    pub(crate) fn dispose_context_page(&mut self, page: &TabHandle) {
        if let Some(cdp) = self.browser_cdp.as_mut() {
            let timeout = Duration::from_secs(3);
            let _ = cdp.call(
                "Target.closeTarget",
                &format!("{{\"targetId\":{}}}", json::string(&page.target_id)),
                timeout,
            );
            let _ = cdp.call(
                "Target.disposeBrowserContext",
                &format!(
                    "{{\"browserContextId\":{}}}",
                    json::string(&page.browser_context_id)
                ),
                timeout,
            );
        }
    }
}

impl Drop for ChromeWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── DevTools: hand-rolled CDP client ──────────────────────────────────────────

pub(crate) struct DevTools {
    pub(crate) ws: WebSocket,
    pub(crate) next_id: u64,
    pub(crate) attempted_authentications: Vec<String>,
    pub(crate) headless: bool,
    pub(crate) interception_enabled: bool,
}

impl DevTools {
    pub(crate) fn connect(url: &str, timeout: Duration, headless: bool) -> Result<Self, String> {
        Ok(Self {
            ws: WebSocket::connect(url, timeout)?,
            next_id: 1,
            attempted_authentications: Vec::new(),
            headless,
            interception_enabled: false,
        })
    }

    fn read_message(&mut self, deadline: Instant) -> Result<String, String> {
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| "CDP read timed out".to_string())?;

            let timeout = if remaining > Duration::from_secs(60) {
                Duration::from_secs(60)
            } else {
                remaining
            };
            self.ws.set_read_timeout(timeout)?;

            let msg = self.ws.read_text()?;

            // Intercept Fetch.requestPaused events to block non-essential resources
            if self.interception_enabled && is_fetch_interception_event(&msg) {
                let _ = handle_fetch_interception(self, &msg);
                continue;
            }
            return Ok(msg);
        }
    }

    pub(crate) fn send_cdp_event(&mut self, method: &str, params: &str) -> Result<(), String> {
        let id = self.next_id;
        self.next_id += 1;
        let message = format!(
            "{{\"id\":{},\"method\":{},\"params\":{}}}",
            id,
            json::string(method),
            params
        );
        self.ws.send_text(&message)
    }

    pub(crate) fn call(
        &mut self,
        method: &str,
        params: &str,
        timeout: Duration,
    ) -> Result<String, String> {
        let id = self.next_id;
        self.next_id += 1;
        let message = format!(
            "{{\"id\":{},\"method\":{},\"params\":{}}}",
            id,
            json::string(method),
            params
        );
        self.ws.send_text(&message)?;

        let deadline = Instant::now() + timeout;
        loop {
            let response = self.read_message(deadline)?;
            if json::has_id(&response, id) {
                if response.contains("\"error\":") {
                    return Err(format!("CDP call failed for {}: {}", method, response));
                }
                return Ok(response);
            }
        }
    }

    pub(crate) fn call_burst<'a, I>(&mut self, calls: I, timeout: Duration) -> Result<(), String>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut pending: Vec<u64> = Vec::new();
        for (method, params) in calls {
            let id = self.next_id;
            self.next_id += 1;
            let message = format!(
                "{{\"id\":{},\"method\":{},\"params\":{}}}",
                id,
                json::string(method),
                params
            );
            self.ws.send_text(&message)?;
            pending.push(id);
        }

        let deadline = Instant::now() + timeout;
        while !pending.is_empty() {
            let response = self.read_message(deadline)?;
            if let Ok(value) = json::parse(&response) {
                if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
                    if let Some(pos) = pending.iter().position(|p| *p == id) {
                        pending.swap_remove(pos);
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn wait_for_event(&mut self, method: &str, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            let message = self.read_message(deadline)?;
            let Ok(value) = json::parse(&message) else {
                continue;
            };
            if value.get("method").and_then(|value| value.as_str()) == Some(method) {
                return Ok(());
            }
        }
    }

    pub(crate) fn evaluate_string_with_timeout(
        &mut self,
        expression: &str,
        timeout: Duration,
    ) -> Result<String, String> {
        let response = self.call(
            "Runtime.evaluate",
            &format!(
                "{{\"expression\":{},\"returnByValue\":true,\"awaitPromise\":true}}",
                json::string(expression)
            ),
            timeout,
        )?;
        json::find_string(&response, "result.result.value")
            .ok_or_else(|| format!("Runtime.evaluate did not return string value: {}", response))
    }

    pub(crate) fn evaluate_value(
        &mut self,
        expression: &str,
        await_promise: bool,
        timeout: Duration,
    ) -> Result<Value, String> {
        let params = format!(
            "{{\"expression\":{},\"returnByValue\":true,\"awaitPromise\":{}}}",
            json::string(expression),
            if await_promise { "true" } else { "false" }
        );
        let response = self.call("Runtime.evaluate", &params, timeout)?;
        let value: Value =
            json::parse(&response).map_err(|err| format!("invalid CDP response JSON: {}", err))?;
        let result = value
            .get("result")
            .ok_or_else(|| "Runtime.evaluate response missing result".to_string())?;
        if let Some(exception) = result.get("exceptionDetails") {
            return Err(format!("Runtime.evaluate threw: {}", exception));
        }
        result
            .get("result")
            .ok_or_else(|| "Runtime.evaluate result missing value".to_string())?
            .get("value")
            .cloned()
            .ok_or_else(|| "Runtime.evaluate value missing".to_string())
    }

    // simple click: move + press + hold + release
    pub(crate) fn click(&mut self, x: f64, y: f64, timeout: Duration) -> Result<(), String> {
        let move_params = format!(
            "{{\"type\":\"mouseMoved\",\"modifiers\":0,\"buttons\":0,\"button\":\"none\",\"x\":{},\"y\":{}}}",
            x, y
        );
        self.call("Input.dispatchMouseEvent", &move_params, timeout)?;

        let press_params = format!(
            "{{\"type\":\"mousePressed\",\"modifiers\":0,\"clickCount\":1,\"buttons\":1,\"button\":\"left\",\"x\":{},\"y\":{}}}",
            x, y
        );
        self.call("Input.dispatchMouseEvent", &press_params, timeout)?;

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos())
            .unwrap_or(0);
        std::thread::sleep(Duration::from_millis(50 + (nanos % 50) as u64));

        let release_params = format!(
            "{{\"type\":\"mouseReleased\",\"modifiers\":0,\"clickCount\":1,\"buttons\":0,\"button\":\"left\",\"x\":{},\"y\":{}}}",
            x, y
        );
        self.call("Input.dispatchMouseEvent", &release_params, timeout)?;

        Ok(())
    }

    // Find click target for the Turnstile widget on a managed challenge page.
    // Same approach as the IUAM solver: JS-based search for iframe/turnstile elements.
    // Shadow DOM pierce is kept as a fallback for pages that embed Turnstile directly.
    pub(crate) fn find_shadow_challenge_click_target(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<(f64, f64)>, String> {
        // Primary: JS-based approach (proven on IUAM + managed challenge)
        let expression = r#"
            (function() {
                var coords = [];
                function pushRect(rect) {
                    if (!rect || rect.width <= 0 || rect.height <= 0) return;
                    coords.push({
                        x: rect.x + 30,
                        y: rect.y + rect.height / 2
                    });
                }

                var responseElements = document.querySelectorAll('[name="cf-turnstile-response"]');
                if (responseElements.length <= 0) {
                    document.querySelectorAll('div').forEach(function(item) {
                        try {
                            var rect = item.getBoundingClientRect();
                            var css = window.getComputedStyle(item);
                            if (
                                css.margin === '0px' &&
                                css.padding === '0px' &&
                                rect.width > 290 &&
                                rect.width <= 310
                            ) {
                                pushRect(rect);
                            }
                        } catch (e) {}
                    });

                    if (coords.length <= 0) {
                        document.querySelectorAll('div').forEach(function(item) {
                            try {
                                var rect = item.getBoundingClientRect();
                                if (rect.width > 290 && rect.width <= 310) {
                                    pushRect(rect);
                                }
                            } catch (e) {}
                        });
                    }

                    if (coords.length <= 0) {
                        document.querySelectorAll('iframe[src*="challenges.cloudflare.com"]').forEach(function(item) {
                            try {
                                pushRect(item.getBoundingClientRect());
                            } catch (e) {}
                        });
                    }
                } else {
                    responseElements.forEach(function(item) {
                        try {
                            var parent = item.parentElement;
                            if (!parent) return;
                            pushRect(parent.getBoundingClientRect());
                        } catch (e) {}
                    });
                }

                return coords;
            })()
        "#;
        
        match self.evaluate_value(expression, false, timeout.min(Duration::from_millis(500))) {
            Ok(value) => {
                if let Some(items) = value.as_array() {
                    for item in items {
                        if let (Some(x), Some(y)) = (
                            item.get("x").and_then(|v| v.as_f64()),
                            item.get("y").and_then(|v| v.as_f64()),
                        ) {
                            if x.is_finite() && y.is_finite() {
                                return Ok(Some((x, y)));
                            }
                        }
                    }
                }
            }
            Err(_) => {}
        }

        // Fallback: try DOM.querySelector for iframe element
        let root_resp = self.call("DOM.getDocument", "{}", timeout)?;
        let root_id = json::find_number(&root_resp, "nodeId")
            .ok_or_else(|| format!("DOM.getDocument did not return nodeId: {}", root_resp))?
            as i64;

        let iframe_query = format!(
            "{{\"nodeId\":{},\"selector\":{}}}",
            root_id,
            json::string("iframe[src*=\"challenges.cloudflare.com\"]")
        );
        let iframe_resp = self.call("DOM.querySelector", &iframe_query, timeout)?;
        if let Some(iframe_id) = json::find_number(&iframe_resp, "nodeId") {
            if iframe_id > 0.0 {
                let model_params = format!("{{\"nodeId\":{}}}", iframe_id as i64);
                if let Ok(model_resp) = self.call("DOM.getBoxModel", &model_params, timeout) {
                    if let Some(coord) = extract_box_center(&model_resp) {
                        return Ok(Some(coord));
                    }
                }
            }
        }

        Ok(None)
    }
}

// ── Chrome executable detection ───────────────────────────────────────────────

fn browser_product(port: u16, timeout: Duration) -> Result<String, String> {
    let body = devtools_http_request("GET", port, "/json/version", timeout)?;
    parse_browser_product(&body)
}

pub(crate) fn browser_ws_url(port: u16, timeout: Duration) -> Result<String, String> {
    let body = devtools_http_request("GET", port, "/json/version", timeout)?;
    let value: Value =
        json::parse(&body).map_err(|err| format!("DevTools /json/version not JSON: {}", err))?;
    value
        .get("webSocketDebuggerUrl")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("DevTools version missing webSocketDebuggerUrl: {}", body))
}

fn wait_for_version(port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        match devtools_http_request("GET", port, "/json/version", Duration::from_secs(2)) {
            Ok(body) => return parse_version(&body),
            Err(err) => {
                if Instant::now() >= deadline {
                    return Err(format!("Chrome DevTools did not become ready: {}", err));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn parse_version(body: &str) -> Result<(), String> {
    parse_browser_product(body).map(|_| ())
}

fn parse_browser_product(body: &str) -> Result<String, String> {
    let value: Value = json::parse(body)
        .map_err(|err| format!("DevTools /json/version response not JSON: {}", err))?;
    let browser = value
        .get("Browser")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("DevTools version response missing Browser: {}", body))?;
    if browser.trim().is_empty() {
        Err(format!(
            "DevTools version response missing browser name: {}",
            body
        ))
    } else {
        Ok(browser.to_string())
    }
}

fn devtools_http_request(
    method: &str,
    port: u16,
    path: &str,
    timeout: Duration,
) -> Result<String, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|err| format!("DevTools HTTP connect failed: {}", err))?;
    stream
        .set_nodelay(true)
        .map_err(|err| format!("failed to set DevTools TCP_NODELAY: {}", err))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|err| format!("failed to set DevTools read timeout: {}", err))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|err| format!("failed to set DevTools write timeout: {}", err))?;

    let request = format!(
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        method, path, port
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("DevTools HTTP write failed: {}", err))?;

    let (head, body) = read_http_response(&mut stream)?;

    if !head.starts_with("HTTP/1.1 200")
        && !head.starts_with("HTTP/1.0 200")
        && !head.starts_with("HTTP/1.1 201")
        && !head.starts_with("HTTP/1.0 201")
    {
        return Err(format!(
            "DevTools HTTP request failed: {}",
            head.lines().next().unwrap_or(&head)
        ));
    }

    Ok(body)
}

fn read_http_response(stream: &mut TcpStream) -> Result<(String, String), String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end;

    loop {
        let read = stream
            .read(&mut chunk)
            .map_err(|err| format!("DevTools HTTP read failed: {}", err))?;
        if read == 0 {
            return Err("DevTools HTTP closed before headers".to_string());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_bytes(&buffer, b"\r\n\r\n") {
            header_end = index + 4;
            break;
        }
        if buffer.len() > 1024 * 1024 {
            return Err("DevTools HTTP headers too large".to_string());
        }
    }

    let head = String::from_utf8_lossy(&buffer[..header_end - 4]).to_string();
    let lower_head = head.to_lowercase();

    if lower_head.contains("transfer-encoding: chunked") {
        loop {
            if let Some(decoded) = try_decode_chunked(&buffer[header_end..])? {
                return String::from_utf8(decoded)
                    .map(|body| (head, body))
                    .map_err(|err| format!("DevTools HTTP body was not utf-8: {}", err));
            }
            let read = stream
                .read(&mut chunk)
                .map_err(|err| format!("DevTools HTTP chunked read failed: {}", err))?;
            if read == 0 {
                return Err("DevTools HTTP chunked body ended early".to_string());
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
    }

    if let Some(content_length) = content_length(&head) {
        while buffer.len() < header_end + content_length {
            let read = stream
                .read(&mut chunk)
                .map_err(|err| format!("DevTools HTTP body read failed: {}", err))?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        let end = buffer.len().min(header_end + content_length);
        return String::from_utf8(buffer[header_end..end].to_vec())
            .map(|body| (head, body))
            .map_err(|err| format!("DevTools HTTP body was not utf-8: {}", err));
    }

    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(_) => break,
        }
    }

    String::from_utf8(buffer[header_end..].to_vec())
        .map(|body| (head, body))
        .map_err(|err| format!("DevTools HTTP body was not utf-8: {}", err))
}

fn content_length(head: &str) -> Option<usize> {
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse::<usize>().ok();
        }
    }
    None
}

fn try_decode_chunked(input: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut cursor = 0usize;
    let mut out = Vec::new();

    loop {
        let Some(line_end) = find_bytes(&input[cursor..], b"\r\n") else {
            return Ok(None);
        };
        let size_line = String::from_utf8_lossy(&input[cursor..cursor + line_end]);
        let size_text = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|err| format!("bad chunk size '{}': {}", size_text, err))?;
        cursor += line_end + 2;

        if size == 0 {
            if input.len() >= cursor + 2 {
                return Ok(Some(out));
            }
            return Ok(None);
        }

        if input.len() < cursor + size + 2 {
            return Ok(None);
        }
        out.extend_from_slice(&input[cursor..cursor + size]);
        cursor += size + 2;
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn find_chrome_executable() -> Result<PathBuf, String> {
    for name in ["CHROME_BIN", "CHROME_PATH"] {
        if let Ok(value) = env::var(name) {
            let path = PathBuf::from(value);
            if path.exists() {
                return Ok(path);
            }
        }
    }

    let mut candidates = Vec::new();

    if cfg!(target_os = "windows") {
        if let Ok(program_files) = env::var("ProgramFiles") {
            candidates
                .push(PathBuf::from(&program_files).join("Google/Chrome/Application/chrome.exe"));
            candidates.push(PathBuf::from(&program_files).join("Chromium/Application/chrome.exe"));
            candidates
                .push(PathBuf::from(&program_files).join("Microsoft/Edge/Application/msedge.exe"));
        }
        if let Ok(program_files_x86) = env::var("ProgramFiles(x86)") {
            candidates.push(
                PathBuf::from(&program_files_x86).join("Google/Chrome/Application/chrome.exe"),
            );
        }
        if let Ok(local_app_data) = env::var("LOCALAPPDATA") {
            candidates
                .push(PathBuf::from(&local_app_data).join("Google/Chrome/Application/chrome.exe"));
        }
    } else if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ));
        candidates.push(PathBuf::from(
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ));
    } else {
        for path in [
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/snap/bin/chromium",
        ] {
            candidates.push(PathBuf::from(path));
        }
    }

    for local in ["chrome", "chromium", "rust/chrome", "../rust/chrome"] {
        let path = PathBuf::from(local);
        if path.exists() {
            if let Some(found) = find_named_executable(&path, 0) {
                return Ok(found);
            }
        }
    }

    if let Some(home) = env::var("USERPROFILE")
        .or_else(|_| env::var("HOME"))
        .ok()
        .map(PathBuf::from)
    {
        let puppeteer_cache = home.join(".cache").join("puppeteer");
        if puppeteer_cache.exists() {
            if let Some(found) = find_named_executable(&puppeteer_cache, 0) {
                return Ok(found);
            }
        }
    }

    for candidate in candidates {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err("No Chrome or Chromium executable found. Set CHROME_BIN to the browser path.".to_string())
}

fn find_named_executable(dir: &Path, depth: usize) -> Option<PathBuf> {
    if depth > 8 {
        return None;
    }
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let name = path.file_name()?.to_string_lossy().to_lowercase();
            if name == "chrome" || name == "chrome.exe" || name == "chromium" {
                return Some(path);
            }
        }
    }

    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_named_executable(&path, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

fn free_port() -> Result<u16, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|err| format!("failed to find free port: {}", err))?;
    listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|err| format!("failed to read free port: {}", err))
}

fn extract_box_center(model_resp: &str) -> Option<(f64, f64)> {
    let model: Value = json::parse(model_resp).ok()?;
    let content = model
        .get("model")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())?;

    if content.len() < 6 {
        return None;
    }

    let mut xs = [0f64; 3];
    let mut ys = [0f64; 3];
    for (i, item) in content[..6].iter().enumerate() {
        let value = item.as_f64()?;
        if i % 2 == 0 {
            xs[i / 2] = value;
        } else {
            ys[i / 2] = value;
        }
    }
    let (min_x, max_x) = xs
        .iter()
        .copied()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(v), hi.max(v))
        });
    let (min_y, max_y) = ys
        .iter()
        .copied()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(v), hi.max(v))
        });
    let cx = (min_x + max_x) / 2.0;
    let cy = (min_y + max_y) / 2.0;
    if !cx.is_finite() || !cy.is_finite() {
        return None;
    }
    Some((cx + 30.0, cy))
}

// Linux: get effective UID for root check (Docker containers run as root)
#[cfg(target_os = "linux")]
extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

// Windows: no-op stub (never called on Windows, but needed for compilation)
#[cfg(not(target_os = "linux"))]
unsafe fn libc_geteuid() -> u32 {
    0
}

// ── Fetch interception for managed challenge ─────────────────────────────────
// Strategy: intercept ONLY non-essential resource types (image, font, media, stylesheet).
// Essential types (document, script, xhr, fetch) are NOT intercepted — they bypass
// the CDP round-trip entirely, so zero overhead under load.
// This cuts image/font/CSS decoding CPU without adding per-request CDP latency.

fn is_fetch_interception_event(input: &str) -> bool {
    if !input.contains("\"method\":\"Fetch.") {
        return false;
    }
    let Ok(value) = json::parse(input) else {
        return false;
    };
    matches!(
        value.get("method").and_then(|m| m.as_str()),
        Some("Fetch.requestPaused" | "Fetch.authRequired")
    )
}

fn handle_fetch_interception(cdp: &mut DevTools, msg: &str) -> Result<(), String> {
    let value = json::parse(msg).map_err(|e| format!("invalid Fetch event: {}", e))?;
    let params = value.get("params").unwrap_or(&value);

    let request_id = params
        .get("requestId")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if request_id.is_empty() {
        return Ok(());
    }

    // Auth challenge
    if params.get("authChallenge").is_some() {
        let params_str = format!(
            "{{\"requestId\":{},\"authChallengeResponse\":{{\"response\":\"Default\"}}}}",
            json::string(request_id)
        );
        return cdp.send_cdp_event("Fetch.continueWithAuth", &params_str);
    }

    let resource_type = params
        .get("resourceType")
        .and_then(|r| r.as_str())
        .unwrap_or("");

    // Only block non-essential resource types — everything else passes through with no CDP overhead
    // since Fetch.enable patterns are scoped to these types only
    let should_block = matches!(
        resource_type.to_ascii_lowercase().as_str(),
        "image" | "font" | "media" | "stylesheet"
    );

    if should_block {
        let params_str = format!(
            "{{\"requestId\":{},\"errorReason\":\"Failed\"}}",
            json::string(request_id)
        );
        cdp.send_cdp_event("Fetch.failRequest", &params_str)
    } else {
        let params_str = format!("{{\"requestId\":{}}}", json::string(request_id));
        cdp.send_cdp_event("Fetch.continueRequest", &params_str)
    }
}
