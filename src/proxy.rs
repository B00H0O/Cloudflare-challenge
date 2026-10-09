use crate::base64;
use crate::server::json;
use crate::server::json::Value;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl ProxyConfig {
    pub fn chrome_proxy_server(&self) -> String {
        format!(
            "{}://{}:{}",
            self.scheme,
            display_host(&self.host),
            self.port
        )
    }

    // chrome can't drive socks or proxy-side https here, route through the bridge
    pub fn requires_bridge(&self) -> bool {
        self.scheme.starts_with("socks") || self.scheme == "https"
    }
}

pub fn parse_from_request_body(body: &str) -> Result<Option<ProxyConfig>, String> {
    let value: Value = json::parse(body).map_err(|err| format!("invalid JSON body: {}", err))?;
    let Some(proxy) = value.get("proxy") else {
        return Ok(None);
    };
    if matches!(proxy, Value::Null) {
        return Ok(None);
    }
    if let Some(spec) = proxy.as_str() {
        return parse_proxy_spec(spec).map(Some);
    }
    if proxy.is_object() {
        return parse_proxy_object(proxy).map(Some);
    }
    Err("proxy must be a string or object".to_string())
}

fn parse_proxy_object(value: &Value) -> Result<ProxyConfig, String> {
    let host = value
        .get("host")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "proxy object must include host".to_string())?
        .to_string();
    let port = value
        .get("port")
        .and_then(|v| match v {
            Value::String(s) => parse_port(s),
            Value::Number(n) => parse_number_port(*n),
            _ => None,
        })
        .ok_or_else(|| "proxy object must include a valid port".to_string())?;
    let scheme = value
        .get("protocol")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("scheme").and_then(|v| v.as_str()))
        .unwrap_or("http");

    let mut config = parse_proxy_parts(scheme, &host, port)?;
    config.username = value
        .get("username")
        .and_then(|v| v.as_str())
        .map(String::from);
    config.password = value
        .get("password")
        .and_then(|v| v.as_str())
        .map(String::from);
    Ok(config)
}

fn parse_proxy_spec(spec: &str) -> Result<ProxyConfig, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("proxy cannot be empty".to_string());
    }

    let (scheme, rest) = match spec.find("://") {
        Some(index) => (&spec[..index], &spec[index + 3..]),
        None => ("http", spec),
    };

    let rest = rest
        .split_once('/')
        .map(|(authority, _)| authority)
        .unwrap_or(rest);

    let (credentials, authority) = match rest.rsplit_once('@') {
        Some((credentials, authority)) => (Some(credentials), authority),
        None => (None, rest),
    };

    let (host, port, inline_credentials) = parse_authority(authority)?;
    let mut config = parse_proxy_parts(scheme, &host, port)?;

    if let Some(credentials) = credentials.or(inline_credentials.as_deref()) {
        let (username, password) = split_credentials(credentials);
        config.username = Some(percent_decode(username));
        config.password = password.map(percent_decode);
    }

    Ok(config)
}

fn parse_proxy_parts(scheme: &str, host: &str, port: u16) -> Result<ProxyConfig, String> {
    let scheme = normalize_scheme(scheme)?;
    let host = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();

    if host.is_empty() {
        return Err("proxy host cannot be empty".to_string());
    }

    Ok(ProxyConfig {
        scheme,
        host,
        port,
        username: None,
        password: None,
    })
}

fn normalize_scheme(scheme: &str) -> Result<String, String> {
    match scheme.trim().to_ascii_lowercase().as_str() {
        "" | "http" => Ok("http".to_string()),
        "https" => Ok("https".to_string()),
        "socks" | "socks5" => Ok("socks5".to_string()),
        "socks4" => Ok("socks4".to_string()),
        other => Err(format!("unsupported proxy protocol '{}'", other)),
    }
}

fn parse_authority(authority: &str) -> Result<(String, u16, Option<String>), String> {
    if authority.starts_with('[') {
        let end = authority
            .find(']')
            .ok_or_else(|| "invalid IPv6 proxy host".to_string())?;
        let host = authority[1..end].to_string();
        let rest = authority[end + 1..]
            .strip_prefix(':')
            .ok_or_else(|| "proxy URL must include a port after the IPv6 host".to_string())?;
        let port = parse_port(rest).ok_or_else(|| "proxy port is invalid".to_string())?;
        return Ok((host, port, None));
    }

    let parts = authority.split(':').collect::<Vec<_>>();
    if parts.len() >= 4 {
        let host = parts[0].to_string();
        let port = parse_port(parts[1]).ok_or_else(|| "proxy port is invalid".to_string())?;
        let credentials = parts[2..].join(":");
        return Ok((host, port, Some(credentials)));
    }

    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| "proxy URL must include host and port".to_string())?;
    let port = parse_port(port).ok_or_else(|| "proxy port is invalid".to_string())?;
    Ok((host.to_string(), port, None))
}

fn parse_port(value: &str) -> Option<u16> {
    let port = value.trim().parse::<u16>().ok()?;
    if port == 0 {
        None
    } else {
        Some(port)
    }
}

fn parse_number_port(value: f64) -> Option<u16> {
    if !value.is_finite() || value.fract() != 0.0 || value <= 0.0 || value > u16::MAX as f64 {
        return None;
    }
    Some(value as u16)
}

fn split_credentials(credentials: &str) -> (&str, Option<&str>) {
    match credentials.split_once(':') {
        Some((username, password)) => (username, Some(password)),
        None => (credentials, None),
    }
}

fn percent_decode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(hex) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                out.push(hex as char);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index] as char);
        index += 1;
    }

    out
}

fn display_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{}]", host)
    } else {
        host.to_string()
    }
}

// loopback proxy chrome connects to: speaks http-proxy to chrome and does the
// upstream hop (socks auth, proxy https, basic auth) chrome won't do itself
pub struct LocalProxyBridge {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LocalProxyBridge {
    pub fn start(upstream: ProxyConfig, timeout: Duration) -> Result<Self, String> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|err| format!("failed to bind local proxy bridge: {}", err))?;
        listener
            .set_nonblocking(true)
            .map_err(|err| format!("failed to configure local proxy bridge: {}", err))?;
        let addr = listener
            .local_addr()
            .map_err(|err| format!("failed to read local proxy bridge address: {}", err))?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);

        let thread = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let upstream = upstream.clone();
                        thread::spawn(move || {
                            if let Err(err) = handle_client(stream, upstream, timeout) {
                                eprintln!("[Proxy] {}", err);
                            }
                        });
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(25));
                    }
                    Err(err) => {
                        eprintln!("[Proxy] local bridge accept failed: {}", err);
                        break;
                    }
                }
            }
        });

        Ok(Self {
            addr,
            shutdown,
            thread: Some(thread),
        })
    }

    pub fn chrome_proxy_server(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for LocalProxyBridge {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // wake a blocked accept so it sees the flag and breaks
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct HttpHead {
    header: Vec<u8>,
    pending: Vec<u8>,
}

fn handle_client(
    mut client: TcpStream,
    upstream: ProxyConfig,
    timeout: Duration,
) -> Result<(), String> {
    set_timeouts(&mut client, timeout)?;
    let head = read_http_head(&mut client)?;
    let header_text = String::from_utf8_lossy(&head.header);
    let request_line = header_text
        .lines()
        .next()
        .ok_or_else(|| "empty proxy request".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "proxy request missing method".to_string())?;
    let target = parts
        .next()
        .ok_or_else(|| "proxy request missing target".to_string())?;

    if method.eq_ignore_ascii_case("CONNECT") {
        handle_connect(client, &upstream, target, &head.pending, timeout)
    } else {
        handle_http_request(client, &upstream, &header_text, &head.pending, timeout)
    }
}

// CONNECT: chrome does its own tls inside the tunnel, we never see it
fn handle_connect(
    mut client: TcpStream,
    upstream: &ProxyConfig,
    target: &str,
    pending: &[u8],
    timeout: Duration,
) -> Result<(), String> {
    let (host, port) = parse_host_port_default(target, 443)?;
    let mut remote = connect_to_target(upstream, &host, port, timeout)?;
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .map_err(|err| format!("failed to confirm CONNECT: {}", err))?;
    if !pending.is_empty() {
        remote
            .write_all(pending)
            .map_err(|err| format!("failed to forward buffered CONNECT bytes: {}", err))?;
    }
    tunnel(client, remote)
}

fn handle_http_request(
    client: TcpStream,
    upstream: &ProxyConfig,
    header_text: &str,
    pending: &[u8],
    timeout: Duration,
) -> Result<(), String> {
    let (host, port, path) = target_from_request(header_text)?;
    let mut remote = if upstream.scheme == "http" || upstream.scheme == "https" {
        let mut stream = TcpStream::connect((upstream.host.as_str(), upstream.port))
            .map_err(|err| format!("failed to connect upstream proxy: {}", err))?;
        set_timeouts(&mut stream, timeout)?;
        let header = rewrite_header_for_upstream_proxy(header_text, upstream)?;
        stream
            .write_all(header.as_bytes())
            .map_err(|err| format!("failed to write upstream proxy request: {}", err))?;
        stream
    } else {
        // socks: we connect to the origin, so rewrite to origin-form
        let mut stream = connect_to_target(upstream, &host, port, timeout)?;
        let header = rewrite_header_for_origin(header_text, &path)?;
        stream
            .write_all(header.as_bytes())
            .map_err(|err| format!("failed to write proxied request: {}", err))?;
        stream
    };

    if !pending.is_empty() {
        remote
            .write_all(pending)
            .map_err(|err| format!("failed to forward buffered request bytes: {}", err))?;
    }
    tunnel(client, remote)
}

fn connect_to_target(
    upstream: &ProxyConfig,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<TcpStream, String> {
    match upstream.scheme.as_str() {
        "http" | "https" => connect_via_http_proxy(upstream, host, port, timeout),
        "socks5" => connect_via_socks5(upstream, host, port, timeout),
        "socks4" => connect_via_socks4(upstream, host, port, timeout),
        other => Err(format!("unsupported proxy protocol '{}'", other)),
    }
}

fn connect_via_http_proxy(
    upstream: &ProxyConfig,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<TcpStream, String> {
    let mut stream = TcpStream::connect((upstream.host.as_str(), upstream.port))
        .map_err(|err| format!("failed to connect upstream proxy: {}", err))?;
    set_timeouts(&mut stream, timeout)?;

    let mut request = format!(
        "CONNECT {}:{} HTTP/1.1\r\nHost: {}:{}\r\nProxy-Connection: keep-alive\r\n",
        display_host(host),
        port,
        display_host(host),
        port
    );
    if let Some(header) = proxy_authorization(upstream) {
        request.push_str(&header);
    }
    request.push_str("\r\n");

    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("failed to write upstream CONNECT: {}", err))?;

    let head = read_http_head(&mut stream)?;
    let status = String::from_utf8_lossy(&head.header)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or_else(|| "upstream proxy returned an invalid CONNECT response".to_string())?;

    if !(200..300).contains(&status) {
        return Err(format!(
            "upstream proxy CONNECT failed with HTTP {}",
            status
        ));
    }

    Ok(stream)
}

// chrome can't do socks5-with-auth, so we speak the handshake by hand here
fn connect_via_socks5(
    upstream: &ProxyConfig,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<TcpStream, String> {
    let mut stream = TcpStream::connect((upstream.host.as_str(), upstream.port))
        .map_err(|err| format!("failed to connect SOCKS5 proxy: {}", err))?;
    set_timeouts(&mut stream, timeout)?;

    if upstream.username.is_some() || upstream.password.is_some() {
        stream
            .write_all(&[0x05, 0x02, 0x00, 0x02])
            .map_err(|err| format!("failed to write SOCKS5 greeting: {}", err))?;
    } else {
        stream
            .write_all(&[0x05, 0x01, 0x00])
            .map_err(|err| format!("failed to write SOCKS5 greeting: {}", err))?;
    }

    let mut greeting = [0u8; 2];
    stream
        .read_exact(&mut greeting)
        .map_err(|err| format!("failed to read SOCKS5 greeting: {}", err))?;
    if greeting[0] != 0x05 {
        return Err("SOCKS5 proxy returned an invalid version".to_string());
    }
    match greeting[1] {
        0x00 => {}
        0x02 => authenticate_socks5(&mut stream, upstream)?,
        0xff => return Err("SOCKS5 proxy rejected all auth methods".to_string()),
        method => {
            return Err(format!(
                "SOCKS5 proxy selected unsupported auth method {}",
                method
            ))
        }
    }

    let mut request = vec![0x05, 0x01, 0x00];
    append_socks_address(&mut request, host)?;
    request.extend_from_slice(&port.to_be_bytes());
    stream
        .write_all(&request)
        .map_err(|err| format!("failed to write SOCKS5 connect request: {}", err))?;

    let mut response = [0u8; 4];
    stream
        .read_exact(&mut response)
        .map_err(|err| format!("failed to read SOCKS5 connect response: {}", err))?;
    if response[0] != 0x05 {
        return Err("SOCKS5 proxy returned an invalid connect version".to_string());
    }
    if response[1] != 0x00 {
        return Err(format!("SOCKS5 connect failed with status {}", response[1]));
    }
    read_socks_bound_address(&mut stream, response[3])?;

    Ok(stream)
}

// user/password sub-negotiation, RFC 1929
fn authenticate_socks5(stream: &mut TcpStream, upstream: &ProxyConfig) -> Result<(), String> {
    let username = upstream.username.as_deref().unwrap_or("");
    let password = upstream.password.as_deref().unwrap_or("");
    if username.len() > u8::MAX as usize || password.len() > u8::MAX as usize {
        return Err("SOCKS5 username/password must be 255 bytes or fewer".to_string());
    }

    let mut auth = vec![0x01, username.len() as u8];
    auth.extend_from_slice(username.as_bytes());
    auth.push(password.len() as u8);
    auth.extend_from_slice(password.as_bytes());
    stream
        .write_all(&auth)
        .map_err(|err| format!("failed to write SOCKS5 credentials: {}", err))?;

    let mut response = [0u8; 2];
    stream
        .read_exact(&mut response)
        .map_err(|err| format!("failed to read SOCKS5 auth response: {}", err))?;
    if response != [0x01, 0x00] {
        return Err("SOCKS5 authentication failed".to_string());
    }

    Ok(())
}

fn append_socks_address(out: &mut Vec<u8>, host: &str) -> Result<(), String> {
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        out.push(0x01);
        out.extend_from_slice(&ip.octets());
    } else if let Ok(ip) = host.parse::<Ipv6Addr>() {
        out.push(0x04);
        out.extend_from_slice(&ip.octets());
    } else {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host.len() > u8::MAX as usize {
            return Err("SOCKS5 destination host is too long".to_string());
        }
        out.push(0x03);
        out.push(host.len() as u8);
        out.extend_from_slice(host.as_bytes());
    }
    Ok(())
}

fn read_socks_bound_address(stream: &mut TcpStream, address_type: u8) -> Result<(), String> {
    let length = match address_type {
        0x01 => 4,
        0x03 => {
            let mut len = [0u8; 1];
            stream
                .read_exact(&mut len)
                .map_err(|err| format!("failed to read SOCKS5 bound domain length: {}", err))?;
            len[0] as usize
        }
        0x04 => 16,
        other => return Err(format!("SOCKS5 returned unknown address type {}", other)),
    };
    let mut skip = vec![0u8; length + 2];
    stream
        .read_exact(&mut skip)
        .map_err(|err| format!("failed to read SOCKS5 bound address: {}", err))
}

// socks4/socks4a: literal ipv4 sent normally, else the 4a trick (0.0.0.1 +
// trailing hostname) for the proxy to resolve. username only, no password.
fn connect_via_socks4(
    upstream: &ProxyConfig,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<TcpStream, String> {
    let mut stream = TcpStream::connect((upstream.host.as_str(), upstream.port))
        .map_err(|err| format!("failed to connect SOCKS4 proxy: {}", err))?;
    set_timeouts(&mut stream, timeout)?;

    let ip = host.parse::<Ipv4Addr>().ok();
    let mut request = vec![0x04, 0x01];
    request.extend_from_slice(&port.to_be_bytes());
    request.extend_from_slice(&ip.unwrap_or(Ipv4Addr::new(0, 0, 0, 1)).octets());
    if let Some(username) = upstream.username.as_deref() {
        request.extend_from_slice(username.as_bytes());
    }
    request.push(0x00);
    if ip.is_none() {
        request.extend_from_slice(host.as_bytes());
        request.push(0x00);
    }

    stream
        .write_all(&request)
        .map_err(|err| format!("failed to write SOCKS4 connect request: {}", err))?;
    let mut response = [0u8; 8];
    stream
        .read_exact(&mut response)
        .map_err(|err| format!("failed to read SOCKS4 connect response: {}", err))?;
    if response[1] != 0x5a {
        return Err(format!("SOCKS4 connect failed with status {}", response[1]));
    }

    Ok(stream)
}

fn read_http_head(stream: &mut TcpStream) -> Result<HttpHead, String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let read = stream
            .read(&mut chunk)
            .map_err(|err| format!("failed to read proxy request: {}", err))?;
        if read == 0 {
            return Err("proxy connection closed before headers".to_string());
        }
        buffer.extend_from_slice(&chunk[..read]);

        if let Some(index) = find_bytes(&buffer, b"\r\n\r\n") {
            let header_end = index + 4;
            return Ok(HttpHead {
                header: buffer[..header_end].to_vec(),
                pending: buffer[header_end..].to_vec(),
            });
        }

        if buffer.len() > 1024 * 1024 {
            return Err("proxy request headers too large".to_string());
        }
    }
}

fn rewrite_header_for_upstream_proxy(
    header_text: &str,
    upstream: &ProxyConfig,
) -> Result<String, String> {
    let mut out = String::new();
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| "proxy request missing request line".to_string())?;
    out.push_str(request_line);
    out.push_str("\r\n");

    for line in lines {
        if line.is_empty() {
            break;
        }
        if is_proxy_control_header(line) {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }

    if let Some(header) = proxy_authorization(upstream) {
        out.push_str(&header);
    }
    out.push_str("\r\n");
    Ok(out)
}

fn rewrite_header_for_origin(header_text: &str, path: &str) -> Result<String, String> {
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| "proxy request missing request line".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "proxy request missing method".to_string())?;
    let _target = parts
        .next()
        .ok_or_else(|| "proxy request missing target".to_string())?;
    let version = parts
        .next()
        .ok_or_else(|| "proxy request missing HTTP version".to_string())?;

    let mut out = format!("{} {} {}\r\n", method, path, version);
    for line in lines {
        if line.is_empty() {
            break;
        }
        if is_proxy_control_header(line) {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    Ok(out)
}

fn is_proxy_control_header(line: &str) -> bool {
    let name = line.split_once(':').map(|(name, _)| name.trim());
    matches!(
        name,
        Some(name)
            if name.eq_ignore_ascii_case("proxy-authorization")
                || name.eq_ignore_ascii_case("proxy-connection")
    )
}

fn proxy_authorization(upstream: &ProxyConfig) -> Option<String> {
    let username = upstream.username.as_deref()?;
    let password = upstream.password.as_deref().unwrap_or("");
    let encoded = base64::encode(format!("{}:{}", username, password).as_bytes());
    Some(format!("Proxy-Authorization: Basic {}\r\n", encoded))
}

fn target_from_request(header_text: &str) -> Result<(String, u16, String), String> {
    let request_line = header_text
        .lines()
        .next()
        .ok_or_else(|| "proxy request missing request line".to_string())?;
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "proxy request missing target".to_string())?;

    if let Some(target) = parse_absolute_url(target)? {
        return Ok(target);
    }

    let host = header_value(header_text, "host")
        .ok_or_else(|| "proxy HTTP request missing Host header".to_string())?;
    let (host, port) = parse_host_port_default(&host, 80)?;
    Ok((host, port, target.to_string()))
}

fn parse_absolute_url(target: &str) -> Result<Option<(String, u16, String)>, String> {
    let Some(index) = target.find("://") else {
        return Ok(None);
    };
    let scheme = target[..index].to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => return Ok(None),
    };
    let rest = &target[index + 3..];
    let path_start = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..path_start];
    let path = if path_start < rest.len() {
        &rest[path_start..]
    } else {
        "/"
    };
    let (host, port) = parse_host_port_default(authority, default_port)?;
    Ok(Some((host, port, path.to_string())))
}

fn header_value(header_text: &str, expected_name: &str) -> Option<String> {
    for line in header_text.lines().skip(1) {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case(expected_name) {
            return Some(value.trim().to_string());
        }
    }
    None
}

fn parse_host_port_default(authority: &str, default_port: u16) -> Result<(String, u16), String> {
    if authority.starts_with('[') {
        let end = authority
            .find(']')
            .ok_or_else(|| "invalid IPv6 authority".to_string())?;
        let host = authority[1..end].to_string();
        let port = authority[end + 1..]
            .strip_prefix(':')
            .and_then(parse_port)
            .unwrap_or(default_port);
        return Ok((host, port));
    }

    match authority.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            let port = parse_port(port).ok_or_else(|| "invalid authority port".to_string())?;
            Ok((host.to_string(), port))
        }
        _ => Ok((authority.to_string(), default_port)),
    }
}

fn set_timeouts(stream: &mut TcpStream, timeout: Duration) -> Result<(), String> {
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|err| format!("failed to set proxy read timeout: {}", err))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|err| format!("failed to set proxy write timeout: {}", err))
}

fn tunnel(client: TcpStream, remote: TcpStream) -> Result<(), String> {
    let mut client_read = client
        .try_clone()
        .map_err(|err| format!("failed to clone client stream: {}", err))?;
    let mut client_write = client;
    let mut remote_read = remote
        .try_clone()
        .map_err(|err| format!("failed to clone remote stream: {}", err))?;
    let mut remote_write = remote;

    let client_to_remote = thread::spawn(move || {
        let _ = std::io::copy(&mut client_read, &mut remote_write);
        let _ = remote_write.shutdown(Shutdown::Write);
    });

    let _ = std::io::copy(&mut remote_read, &mut client_write);
    let _ = client_write.shutdown(Shutdown::Write);
    let _ = client_to_remote.join();
    Ok(())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}