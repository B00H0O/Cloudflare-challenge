// minimal websocket client, no deps. just enough to talk to chrome's devtools
// endpoint: http upgrade handshake then raw frame read/write. not general-purpose
// (assumes ws://, text/control frames, server doesn't mask).
use crate::base64;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct WebSocket {
    stream: TcpStream,
    // growable read buffer; pos/end mark the unconsumed window so frames that span
    // multiple reads stitch together without reallocating per call
    read_buf: Vec<u8>,
    read_pos: usize,
    read_end: usize,
}

struct WsUrl {
    host: String,
    port: u16,
    path: String,
}

impl WebSocket {
    pub fn connect(url: &str, timeout: Duration) -> Result<Self, String> {
        let parsed = parse_ws_url(url)?;
        let mut stream = TcpStream::connect((parsed.host.as_str(), parsed.port))
            .map_err(|err| format!("failed to connect websocket {}: {}", url, err))?;
        stream
            .set_nodelay(true)
            .map_err(|err| format!("failed to set websocket TCP_NODELAY: {}", err))?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|err| format!("failed to set websocket read timeout: {}", err))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|err| format!("failed to set websocket write timeout: {}", err))?;

        let key = websocket_key();
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
            parsed.path, parsed.host, parsed.port, key
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|err| format!("failed websocket handshake write: {}", err))?;

        // byte-at-a-time reads so we stop exactly at the end of the headers and
        // don't swallow frame bytes that arrive right after the upgrade
        let mut response = Vec::new();
        let mut byte = [0u8; 1];
        while !response.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(0) => return Err("websocket handshake closed early".to_string()),
                Ok(_) => {}
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Err("websocket handshake timed out".to_string());
                }
                Err(err) => return Err(format!("failed websocket handshake read: {}", err)),
            }
            response.push(byte[0]);
            if response.len() > 16 * 1024 {
                return Err("websocket handshake too large".to_string());
            }
        }

        let text = String::from_utf8_lossy(&response);
        if !text.starts_with("HTTP/1.1 101") && !text.starts_with("HTTP/1.0 101") {
            return Err(format!(
                "websocket upgrade failed: {}",
                text.lines().next().unwrap_or("")
            ));
        }

        Ok(Self {
            stream,
            read_buf: Vec::with_capacity(64 * 1024),
            read_pos: 0,
            read_end: 0,
        })
    }

    pub fn set_read_timeout(&mut self, timeout: Duration) -> Result<(), String> {
        self.stream
            .set_read_timeout(Some(timeout))
            .map_err(|err| format!("failed to set websocket read timeout: {}", err))
    }

    pub fn send_text(&mut self, text: &str) -> Result<(), String> {
        self.send_frame(0x1, text.as_bytes())
    }

    // pull frames until we have one complete text message (a message may be
    // split across continuation frames; we append until fin shows up)
    pub fn read_text(&mut self) -> Result<String, String> {
        let mut message = Vec::new();
        loop {
            let frame = self.read_frame()?;
            match frame.opcode {
                0x0 | 0x1 => {
                    message.extend_from_slice(&frame.payload);
                    if frame.fin {
                        return String::from_utf8(message)
                            .map_err(|err| format!("websocket text was not utf-8: {}", err));
                    }
                }
                0x8 => return Err("websocket closed".to_string()),
                0x9 => {
                    // ping -> pong echoing the payload, then keep reading
                    self.send_frame(0xA, &frame.payload)?;
                }
                0xA => {}
                _ => {}
            }
        }
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        // 0x80 = fin bit; we always send the whole payload in one frame
        frame.push(0x80 | (opcode & 0x0f));
        // client frames must be masked per the spec, so the top bit of byte 2 is always set
        let mask_bit = 0x80;
        // length encoding: <126 in the 7 low bits, 126 -> 16-bit length, 127 -> 64-bit
        if payload.len() < 126 {
            frame.push(mask_bit | payload.len() as u8);
        } else if payload.len() <= u16::MAX as usize {
            frame.push(mask_bit | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            frame.push(mask_bit | 127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }

        // 4-byte mask goes on the wire, then each payload byte xor'd with mask[i % 4]
        let mask = websocket_mask();
        frame.extend_from_slice(&mask);
        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[index % 4]);
        }

        self.stream
            .write_all(&frame)
            .map_err(|err| format!("failed websocket frame write: {}", err))
    }

    // make sure `needed` unconsumed bytes sit in the buffer, reading from the socket
    // if short. lets read_frame ask for header/length/payload without caring how
    // the tcp stream chunked them
    fn ensure_bytes(&mut self, needed: usize) -> Result<(), String> {
        if self.read_end - self.read_pos >= needed {
            return Ok(());
        }
        // slide leftover bytes to the front so the buffer doesn't grow forever
        if self.read_pos > 0 {
            self.read_buf.copy_within(self.read_pos..self.read_end, 0);
            self.read_end -= self.read_pos;
            self.read_pos = 0;
        }
        if self.read_buf.len() < self.read_end + needed {
            self.read_buf.resize(self.read_end + needed, 0);
        }
        let mut taken = 0;
        while taken < needed {
            match self
                .stream
                .read(&mut self.read_buf[self.read_end + taken..self.read_end + needed])
            {
                Ok(0) => return Err("websocket closed unexpectedly".to_string()),
                Ok(read) => taken += read,
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Err("websocket read timed out".to_string());
                }
                Err(err) => return Err(format!("failed websocket read: {}", err)),
            }
        }
        self.read_end += taken;
        Ok(())
    }

    fn read_frame(&mut self) -> Result<Frame, String> {
        // first two bytes are always present: fin+opcode, then mask flag + 7-bit length
        self.ensure_bytes(2)?;
        let header = &self.read_buf[self.read_pos..self.read_pos + 2];
        let fin = header[0] & 0x80 != 0;
        let opcode = header[0] & 0x0f;
        let masked = header[1] & 0x80 != 0;
        let mut length = (header[1] & 0x7f) as u64;
        self.read_pos += 2;

        // 126 -> next 2 bytes are the length, 127 -> next 8
        if length == 126 {
            self.ensure_bytes(2)?;
            let extended = &self.read_buf[self.read_pos..self.read_pos + 2];
            length = u16::from_be_bytes([extended[0], extended[1]]) as u64;
            self.read_pos += 2;
        } else if length == 127 {
            self.ensure_bytes(8)?;
            let extended: [u8; 8] = self.read_buf[self.read_pos..self.read_pos + 8]
                .try_into()
                .map_err(|_| "websocket extended length read failed".to_string())?;
            length = u64::from_be_bytes(extended);
            self.read_pos += 8;
        }

        // sanity cap so a bogus length can't make us allocate gigabytes
        if length > 64 * 1024 * 1024 {
            return Err("websocket frame too large".to_string());
        }

        // chrome won't mask its frames, but handle it anyway if the bit ever is set
        let mut mask = [0u8; 4];
        if masked {
            self.ensure_bytes(4)?;
            mask.copy_from_slice(&self.read_buf[self.read_pos..self.read_pos + 4]);
            self.read_pos += 4;
        }

        let payload_len = length as usize;
        self.ensure_bytes(payload_len)?;
        let mut payload = vec![0u8; payload_len];
        payload.copy_from_slice(&self.read_buf[self.read_pos..self.read_pos + payload_len]);
        self.read_pos += payload_len;

        if masked {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }

        Ok(Frame {
            fin,
            opcode,
            payload,
        })
    }
}

struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

fn parse_ws_url(url: &str) -> Result<WsUrl, String> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| format!("only ws:// URLs are supported: {}", url))?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    // rsplit: an ipv6-ish host has colons, so only the last colon is the port
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port = port
                .parse::<u16>()
                .map_err(|err| format!("bad websocket port '{}': {}", port, err))?;
            (host.to_string(), port)
        }
        None => (authority.to_string(), 80),
    };

    Ok(WsUrl {
        host,
        port,
        path: path.to_string(),
    })
}

// Sec-WebSocket-Key: 16 random bytes, base64'd. spec wants randomness but we
// don't verify the server's accept hash, so anything non-repeating works.
fn websocket_key() -> String {
    let mut bytes = [0u8; 16];
    fill_pseudo_random(&mut bytes);
    base64::encode(&bytes)
}

fn websocket_mask() -> [u8; 4] {
    let mut bytes = [0u8; 4];
    fill_pseudo_random(&mut bytes);
    bytes
}

// cheap xorshift seeded from clock + pid. not crypto, doesn't need to be: feeds
// only the handshake key and frame masks, neither a security boundary here.
fn fill_pseudo_random(out: &mut [u8]) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = nanos ^ ((std::process::id() as u64) << 32);
    for byte in out {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = (state & 0xff) as u8;
    }
}