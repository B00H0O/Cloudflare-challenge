mod browser;
mod solver;
mod server;
mod proxy;
mod ws;
mod tui;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn main() {
    load_dotenv();

    let port = env_usize("PORT", 408) as u16;
    let browsers = env_usize("BROWSERS", 2).clamp(1, 16);
    let tabs = env_usize("TABS", 10).clamp(1, 50);
    let timeout = Duration::from_millis(env_usize("timeOut", 90_000) as u64);
    let headless = env_bool("HEADLESS", true);

    tui::banner(port, browsers, tabs);

    let service = Arc::new(solver::SolverPool::new(timeout, browsers, tabs, headless));

    if let Err(err) = shutdown::install() {
        eprintln!("[Warning] Failed to install shutdown handler: {}", err);
    }

    if env_bool("PREWARM_BROWSER", true) {
        let svc = Arc::clone(&service);
        thread::spawn(move || {
            if let Err(err) = svc.prewarm(headless) {
                eprintln!("[Warning] Browser prewarm failed: {}", err);
            }
        });
    }

    if let Err(err) = server::serve(port, Arc::clone(&service)) {
        panic!("HTTP server failed: {}", err);
    }

    service.shutdown();
}

fn load_dotenv() {
    for path in ["./.env", "./config.env", "./cf.env"] {
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines() {
                if line.starts_with('#') || line.trim().is_empty() {
                    continue;
                }
                if let Some((key, value)) = line.split_once('=') {
                    let key = key.trim();
                    let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
                    if std::env::var(key).is_err() {
                        std::env::set_var(key, value);
                    }
                }
            }
            return;
        }
    }
}

fn env_usize(name: &str, default_value: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default_value)
}

fn env_bool(name: &str, default_value: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default_value,
        },
        Err(_) => default_value,
    }
}

mod base64 {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        let mut i = 0;
        while i < input.len() {
            let b0 = input[i];
            let b1 = if i + 1 < input.len() { input[i + 1] } else { 0 };
            let b2 = if i + 2 < input.len() { input[i + 2] } else { 0 };
            out.push(TABLE[(b0 >> 2) as usize] as char);
            out.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
            if i + 1 < input.len() {
                out.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
            } else {
                out.push('=');
            }
            if i + 2 < input.len() {
                out.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
            } else {
                out.push('=');
            }
            i += 3;
        }
        out
    }
}

mod shutdown {
    use std::sync::atomic::{AtomicBool, Ordering};

    static SHUTDOWN: AtomicBool = AtomicBool::new(false);

    pub fn install() -> Result<(), String> {
        platform::install()
    }

    pub fn is_requested() -> bool {
        SHUTDOWN.load(Ordering::SeqCst)
    }

    fn request_shutdown() {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }

    #[cfg(windows)]
    mod platform {
        use super::request_shutdown;
        type HandlerRoutine = unsafe extern "system" fn(u32) -> i32;
        #[link(name = "Kernel32")]
        extern "system" {
            fn SetConsoleCtrlHandler(handler: Option<HandlerRoutine>, add: i32) -> i32;
        }
        pub fn install() -> Result<(), String> {
            let ok = unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
            if ok == 0 { Err("SetConsoleCtrlHandler failed".to_string()) } else { Ok(()) }
        }
        unsafe extern "system" fn handler(ctrl: u32) -> i32 {
            if matches!(ctrl, 0 | 1 | 2 | 5 | 6) { request_shutdown(); 1 } else { 0 }
        }
    }

    #[cfg(unix)]
    mod platform {
        use super::request_shutdown;
        type SignalHandler = extern "C" fn(i32);
        extern "C" { fn signal(signum: i32, handler: SignalHandler) -> SignalHandler; }
        pub fn install() -> Result<(), String> {
            unsafe { signal(2, handler); signal(15, handler); }
            Ok(())
        }
        extern "C" fn handler(_: i32) { request_shutdown(); }
    }

    #[cfg(not(any(unix, windows)))]
    mod platform {
        pub fn install() -> Result<(), String> { Ok(()) }
    }
}
