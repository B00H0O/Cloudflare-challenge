use crate::solver::{SolveJob, SolverPool, SolveError};
use std::env;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

struct HttpRequest {
    method: String,
    path: String,
    body: String,
}

struct HttpResponse {
    status: u16,
    content_type: &'static str,
    body: String,
}

pub fn serve(port: u16, service: Arc<SolverPool>) -> Result<(), String> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .map_err(|err| format!("failed to bind HTTP server on port {}: {}", port, err))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("failed to configure HTTP listener: {}", err))?;

    if env::var("DEBUG").is_ok() {
        println!("[System] CF Managed service running on port {}", port);
    }

    while !crate::shutdown::is_requested() {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(err) = stream.set_nonblocking(false) {
                    eprintln!("[HTTP] failed to set blocking mode: {}", err);
                    continue;
                }
                let service = Arc::clone(&service);
                thread::spawn(move || {
                    if let Err(err) = handle_connection(stream, service) {
                        eprintln!("[HTTP] {}", err);
                    }
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => eprintln!("[HTTP] connection failed: {}", err),
        }
    }

    if env::var("DEBUG").is_ok() {
        println!("[System] Shutting down CF Managed service");
    }
    Ok(())
}

impl HttpRequest {
    fn read_from(stream: &mut TcpStream, timeout: Duration) -> Result<Self, String> {
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|err| format!("failed to set read timeout: {}", err))?;

        let mut buffer = Vec::with_capacity(2048);
        let mut chunk = [0u8; 4096];
        let header_end;

        loop {
            let read = stream
                .read(&mut chunk)
                .map_err(|err| format!("failed to read request: {}", err))?;
            if read == 0 {
                return Err("connection closed before headers".to_string());
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(index) = find_bytes(&buffer, b"\r\n\r\n") {
                header_end = index + 4;
                break;
            }
            if buffer.len() > 1024 * 1024 {
                return Err("request headers too large".to_string());
            }
        }

        let header_text = std::str::from_utf8(&buffer[..header_end])
            .map_err(|_| "request headers not utf-8".to_string())?;
        let mut lines = header_text.split("\r\n");
        let request_line = lines
            .next()
            .ok_or_else(|| "missing request line".to_string())?;
        let mut parts = request_line.split_whitespace();
        let method = parts
            .next()
            .ok_or_else(|| "missing HTTP method".to_string())?
            .to_string();
        let target = parts
            .next()
            .ok_or_else(|| "missing HTTP target".to_string())?;
        let path = target.split('?').next().unwrap_or(target).to_string();

        let mut content_length = 0usize;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().unwrap_or(0);
            }
        }

        while buffer.len() < header_end + content_length {
            let read = stream
                .read(&mut chunk)
                .map_err(|err| format!("failed to read request body: {}", err))?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.len() > header_end + 10 * 1024 * 1024 {
                return Err("request body too large".to_string());
            }
        }

        let body_end = buffer.len().min(header_end + content_length);
        let body = String::from_utf8(buffer[header_end..body_end].to_vec())
            .map_err(|_| "request body not utf-8".to_string())?;

        Ok(Self { method, path, body })
    }
}

impl HttpResponse {
    fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body: body.to_string(),
        }
    }

    fn write_to(&self, stream: &mut TcpStream) -> Result<(), String> {
        let reason = reason_phrase(self.status);
        let dyno = sanitize_header_value(&env::var("DYNO").unwrap_or_else(|_| "local".to_string()));
        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nX-Dyno: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.status,
            reason,
            self.content_type,
            dyno,
            self.body.len(),
            self.body
        );
        stream
            .write_all(response.as_bytes())
            .map_err(|err| format!("failed to write response: {}", err))
    }
}

fn handle_connection(mut stream: TcpStream, service: Arc<SolverPool>) -> Result<(), String> {
    let request = HttpRequest::read_from(&mut stream, service.timeout())?;
    let response = route(request, &service);
    response.write_to(&mut stream)
}

fn route(request: HttpRequest, service: &SolverPool) -> HttpResponse {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/health") => health(service),
        ("POST", "/managed") => solve_managed(request.body, service),
        ("POST", "/v1") => solve_flaresolverr(request.body, service),
        _ => json_error(404, "Not Found"),
    }
}

fn health(service: &SolverPool) -> HttpResponse {
    let (capacity, available, active) = service.capacity_snapshot();
    HttpResponse::json(
        200,
        &json::object(&[
            ("status", json::string("ok")),
            (
                "dyno",
                json::string(&env::var("DYNO").unwrap_or_else(|_| "local".to_string())),
            ),
            ("capacity", capacity.to_string()),
            ("available", available.to_string()),
            ("active", active.to_string()),
        ]),
    )
}

fn solve_managed(body: String, service: &SolverPool) -> HttpResponse {
    let request = match SolveJob::from_json(&body) {
        Ok(request) => request,
        Err(err) => return json_error(400, &err),
    };

    let url = request.url.clone();
    let started = crate::tui::now_hms();
    let start_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    match service.solve(request) {
        Ok(result) => {
            let token = result.cf_clearance.clone().unwrap_or_default();
            crate::tui::log_done(&started, &url, result.elapsed_ms, &token);
            HttpResponse::json(200, &result.to_json())
        }
        Err(SolveError::TooManyRequests) => {
            crate::tui::log_fail(&started, &url, "too many requests");
            json_error(429, "Too Many Requests")
        }
        Err(SolveError::Internal(err)) => {
            crate::tui::log_fail(&started, &url, &err);
            json_error(500, &err)
        }
    }
}

// FlareSolverr-compatible /v1 endpoint: drop-in replacement for FlareSolverr
fn solve_flaresolverr(body: String, service: &SolverPool) -> HttpResponse {
    let cmd = json::find_string(&body, "cmd").unwrap_or_default();
    if cmd != "request.get" {
        return json_error(400, &format!("unsupported cmd: {} (only request.get)", cmd));
    }

    let url = match json::find_string(&body, "url") {
        Some(value) if value.starts_with("http://") || value.starts_with("https://") => value,
        Some(_) => return json_error(400, "url must start with http:// or https://"),
        None => return json_error(400, "missing url parameter"),
    };

    let proxy = match crate::proxy::parse_from_request_body(&body) {
        Ok(p) => p,
        Err(e) => return json_error(400, &e),
    };

    // FlareSolverr maxTimeout (ms): absent/0 -> pool default, clamped to 1s..5min
    let timeout_override = match json::find_number(&body, "maxTimeout") {
        Some(ms) if ms > 0.0 => Some(Duration::from_millis(ms.clamp(1000.0, 300_000.0) as u64)),
        _ => None,
    };

    let request = SolveJob { url: url.clone(), proxy, timeout_override };
    let started = crate::tui::now_hms();
    let start_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    match service.solve(request) {
        Ok(result) => {
            let token = result.cf_clearance.clone().unwrap_or_default();
            crate::tui::log_done(&started, &url, result.elapsed_ms, &token);
            HttpResponse::json(200, &result.to_flaresolverr_json(start_ts))
        }
        Err(SolveError::TooManyRequests) => {
            crate::tui::log_fail(&started, &url, "too many requests");
            json_error(429, "Too Many Requests")
        }
        Err(SolveError::Internal(err)) => {
            crate::tui::log_fail(&started, &url, &err);
            json_error(500, &err)
        }
    }
}

fn json_error(status: u16, message: &str) -> HttpResponse {
    HttpResponse::json(
        status,
        &json::object(&[
            ("success", "false".to_string()),
            ("message", json::string(message)),
        ]),
    )
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn sanitize_header_value(value: &str) -> String {
    value
        .chars()
        .filter(|ch| *ch != '\r' && *ch != '\n')
        .collect()
}

// ── JSON parser (zero-dep, ported from Turnstile Solver) ──────────────────────

pub(crate) mod json {
    #[derive(Clone, Debug)]
    pub enum Value {
        Null,
        Bool(bool),
        Number(f64),
        String(String),
        Array(Vec<Value>),
        Object(Vec<(String, Value)>),
    }

    impl Value {
        pub fn get(&self, key: &str) -> Option<&Value> {
            let Value::Object(fields) = self else {
                return None;
            };
            fields
                .iter()
                .find_map(|(name, value)| if name == key { Some(value) } else { None })
        }

        pub fn as_str(&self) -> Option<&str> {
            match self {
                Value::String(value) => Some(value),
                _ => None,
            }
        }

        pub fn as_f64(&self) -> Option<f64> {
            match self {
                Value::Number(value) => Some(*value),
                _ => None,
            }
        }

        pub fn as_u64(&self) -> Option<u64> {
            match self {
                Value::Number(value) if *value >= 0.0 => Some(*value as u64),
                _ => None,
            }
        }

        pub fn as_array(&self) -> Option<&[Value]> {
            match self {
                Value::Array(values) => Some(values),
                _ => None,
            }
        }

        pub fn is_object(&self) -> bool {
            matches!(self, Value::Object(_))
        }

        pub fn stringify(&self) -> String {
            match self {
                Value::Null => "null".to_string(),
                Value::Bool(true) => "true".to_string(),
                Value::Bool(false) => "false".to_string(),
                Value::Number(value) => {
                    if value.fract() == 0.0 {
                        format!("{:.0}", value)
                    } else {
                        value.to_string()
                    }
                }
                Value::String(value) => string(value),
                Value::Array(values) => {
                    let inner = values
                        .iter()
                        .map(Value::stringify)
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("[{}]", inner)
                }
                Value::Object(fields) => {
                    let inner = fields
                        .iter()
                        .map(|(key, value)| format!("{}:{}", string(key), value.stringify()))
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("{{{}}}", inner)
                }
            }
        }
    }

    impl std::fmt::Display for Value {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(&self.stringify())
        }
    }

    pub fn string(value: &str) -> String {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for ch in value.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{08}' => out.push_str("\\b"),
                '\u{0c}' => out.push_str("\\f"),
                ch if ch < ' ' => out.push_str(&format!("\\u{:04x}", ch as u32)),
                ch => out.push(ch),
            }
        }
        out.push('"');
        out
    }

    pub fn object(fields: &[(&str, String)]) -> String {
        let inner = fields
            .iter()
            .map(|(key, value)| format!("{}:{}", string(key), value))
            .collect::<Vec<_>>()
            .join(",");
        format!("{{{}}}", inner)
    }

    pub fn find_string(input: &str, key: &str) -> Option<String> {
        let value = parse(input).ok()?;
        value_at_path(&value, key).and_then(value_to_string)
    }

    pub fn has_id(input: &str, expected_id: u64) -> bool {
        if let Ok(value) = parse(input) {
            if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
                return id == expected_id;
            }
        }
        false
    }

    pub fn find_number(input: &str, key: &str) -> Option<f64> {
        let value = parse(input).ok()?;
        value_at_path(&value, key)?.as_f64()
    }

    pub fn parse(input: &str) -> Result<Value, String> {
        let mut parser = Parser {
            input: input.as_bytes(),
            pos: 0,
        };
        let value = parser.parse_value()?;
        parser.skip_ws();
        if parser.pos != parser.input.len() {
            return Err("trailing JSON characters".to_string());
        }
        Ok(value)
    }

    fn value_at_path<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
        let mut current = value;
        for part in key.split('.') {
            current = current.get(part)?;
        }
        Some(current)
    }

    pub fn value_to_string(value: &Value) -> Option<String> {
        match value {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => {
                if n.fract() == 0.0 {
                    Some(format!("{:.0}", n))
                } else {
                    Some(n.to_string())
                }
            }
            Value::Bool(b) => Some(b.to_string()),
            Value::Null => None,
            _ => Some(value.stringify()),
        }
    }

    struct Parser<'a> {
        input: &'a [u8],
        pos: usize,
    }

    impl Parser<'_> {
        fn parse_value(&mut self) -> Result<Value, String> {
            self.skip_ws();
            match self.peek() {
                Some(b'"') => self.parse_string().map(Value::String),
                Some(b'{') => self.parse_object(),
                Some(b'[') => self.parse_array(),
                Some(b't') => {
                    self.expect_bytes(b"true")?;
                    Ok(Value::Bool(true))
                }
                Some(b'f') => {
                    self.expect_bytes(b"false")?;
                    Ok(Value::Bool(false))
                }
                Some(b'n') => {
                    self.expect_bytes(b"null")?;
                    Ok(Value::Null)
                }
                Some(b'-' | b'0'..=b'9') => self.parse_number().map(Value::Number),
                _ => Err("unexpected JSON value".to_string()),
            }
        }

        fn parse_object(&mut self) -> Result<Value, String> {
            self.expect(b'{')?;
            let mut fields = Vec::new();
            loop {
                self.skip_ws();
                if self.consume(b'}') {
                    break;
                }
                let key = self.parse_string()?;
                self.skip_ws();
                self.expect(b':')?;
                let value = self.parse_value()?;
                fields.push((key, value));
                self.skip_ws();
                if self.consume(b'}') {
                    break;
                }
                self.expect(b',')?;
            }
            Ok(Value::Object(fields))
        }

        fn parse_array(&mut self) -> Result<Value, String> {
            self.expect(b'[')?;
            let mut values = Vec::new();
            loop {
                self.skip_ws();
                if self.consume(b']') {
                    break;
                }
                values.push(self.parse_value()?);
                self.skip_ws();
                if self.consume(b']') {
                    break;
                }
                self.expect(b',')?;
            }
            Ok(Value::Array(values))
        }

        fn parse_string(&mut self) -> Result<String, String> {
            self.expect(b'"')?;
            let mut out = String::new();
            while let Some(byte) = self.next() {
                match byte {
                    b'"' => return Ok(out),
                    b'\\' => {
                        let escaped = self.next().ok_or_else(|| "bad JSON escape".to_string())?;
                        match escaped {
                            b'"' => out.push('"'),
                            b'\\' => out.push('\\'),
                            b'/' => out.push('/'),
                            b'b' => out.push('\u{08}'),
                            b'f' => out.push('\u{0c}'),
                            b'n' => out.push('\n'),
                            b'r' => out.push('\r'),
                            b't' => out.push('\t'),
                            b'u' => {
                                let code = self.parse_hex4()?;
                                if let Some(ch) = char::from_u32(code) {
                                    out.push(ch);
                                }
                            }
                            _ => return Err("bad JSON escape".to_string()),
                        }
                    }
                    _ => out.push(byte as char),
                }
            }
            Err("unterminated JSON string".to_string())
        }

        fn parse_number(&mut self) -> Result<f64, String> {
            let start = self.pos;
            if self.peek() == Some(b'-') {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.peek() == Some(b'.') {
                self.pos += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            if matches!(self.peek(), Some(b'e' | b'E')) {
                self.pos += 1;
                if matches!(self.peek(), Some(b'+' | b'-')) {
                    self.pos += 1;
                }
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            std::str::from_utf8(&self.input[start..self.pos])
                .ok()
                .and_then(|text| text.parse::<f64>().ok())
                .ok_or_else(|| "bad JSON number".to_string())
        }

        fn parse_hex4(&mut self) -> Result<u32, String> {
            if self.pos + 4 > self.input.len() {
                return Err("short JSON unicode escape".to_string());
            }
            let text = std::str::from_utf8(&self.input[self.pos..self.pos + 4])
                .map_err(|_| "bad JSON unicode escape".to_string())?;
            self.pos += 4;
            u32::from_str_radix(text, 16).map_err(|_| "bad JSON unicode escape".to_string())
        }

        fn skip_ws(&mut self) {
            while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
                self.pos += 1;
            }
        }

        fn expect_bytes(&mut self, expected: &[u8]) -> Result<(), String> {
            if self.input.get(self.pos..self.pos + expected.len()) == Some(expected) {
                self.pos += expected.len();
                Ok(())
            } else {
                Err("unexpected JSON token".to_string())
            }
        }

        fn expect(&mut self, expected: u8) -> Result<(), String> {
            if self.consume(expected) {
                Ok(())
            } else {
                Err("unexpected JSON character".to_string())
            }
        }

        fn consume(&mut self, expected: u8) -> bool {
            if self.peek() == Some(expected) {
                self.pos += 1;
                true
            } else {
                false
            }
        }

        fn peek(&self) -> Option<u8> {
            self.input.get(self.pos).copied()
        }

        fn next(&mut self) -> Option<u8> {
            let byte = self.peek()?;
            self.pos += 1;
            Some(byte)
        }
    }
}
