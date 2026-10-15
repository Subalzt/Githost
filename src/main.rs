//! Githost: folders kept the same on every laptop on the Wi-Fi, laptop to laptop with nothing
//! in between, and a GitHub repository behind each one that anyone commits to with one button.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
mod api;
mod git;
mod net;
mod pair;
mod store;
mod sync;

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::Mac;
use sha2::{Digest, Sha256};

pub const PORT: u16 = 47800;

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

pub fn random(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    getrandom::getrandom(&mut v).expect("no randomness");
    v
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Compares in time that does not depend on where the two differ.
pub fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn sha(s: &str) -> Vec<u8> {
    Sha256::digest(s.as_bytes()).to_vec()
}

pub fn hmac(key: &[u8], msg: &str) -> String {
    let mut m = hmac::Hmac::<Sha256>::new_from_slice(key).unwrap();
    m.update(msg.as_bytes());
    hex(&m.finalize().into_bytes())
}

pub fn mtime(md: &fs::Metadata) -> i64 {
    md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64)
}

/// Reads `r` (up to `len` bytes), writing it to `w` when given, and returns the SHA-256 of it.
pub fn hash_copy(r: &mut dyn Read, mut w: Option<&mut File>, len: Option<u64>) -> io::Result<String> {
    let (mut h, mut buf, mut left) = (Sha256::new(), vec![0u8; 1 << 20], len.unwrap_or(u64::MAX));
    while left > 0 {
        let n = r.read(&mut buf[..left.min(1 << 20) as usize])?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        if let Some(w) = w.as_mut() {
            w.write_all(&buf[..n])?;
        }
        left -= n as u64;
    }
    if len.is_some() && left > 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "cut off"));
    }
    Ok(hex(&h.finalize()))
}

pub fn hash_file(p: &Path) -> io::Result<String> {
    hash_copy(&mut File::open(p)?, None, None)
}

/// A path inside the folder, from the / form used everywhere else.
pub fn folder_path(root: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(root.to_path_buf(), |p, s| p.join(s))
}

pub fn url_decode(s: &str) -> String {
    let (b, mut out, mut i) = (s.as_bytes(), Vec::new(), 0);
    while i < b.len() {
        let byte = (b[i] == b'%' && i + 2 < b.len()).then(|| u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).ok()?, 16).ok());
        match byte.flatten() {
            Some(v) => (out.push(v), i += 3),
            None => (out.push(if b[i] == b'+' { b' ' } else { b[i] }), i += 1),
        };
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn url_encode(s: &str) -> String {
    s.bytes().map(|c| if c.is_ascii_alphanumeric() || b"-_.~/".contains(&c) { (c as char).to_string() } else { format!("%{:02X}", c) }).collect()
}

/// A command that never flashes a console window on Windows.
pub fn quiet(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000);
    c
}

static LOG: Mutex<Option<File>> = Mutex::new(None);

pub fn log_line(s: &str) {
    let line = format!("{} {}\n", now_ms(), s);
    eprint!("{}", line);
    if let Some(f) = LOG.lock().unwrap().as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::log_line(&format!($($t)*)) };
}

fn data_dir() -> PathBuf {
    // Settings from before the app was called Githost move over the first time.
    let (old, new) = match std::env::var("APPDATA") {
        Ok(a) => (PathBuf::from(&a).join("FolderSync"), PathBuf::from(a).join("Githost")),
        Err(_) => {
            let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
            match cfg!(target_os = "macos") {
                true => (home.join("Library/Application Support/FolderSync"), home.join("Library/Application Support/Githost")),
                false => (home.join(".config/foldersync"), home.join(".config/githost")),
            }
        }
    };
    if old.is_dir() && !new.exists() {
        let _ = fs::rename(&old, &new);
    }
    new
}

fn main() {
    // --data DIR, --port N, --disc N, --disc-send N,N (to run two on one laptop), --no-window
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let data = arg("--data").map(PathBuf::from).unwrap_or_else(data_dir);
    let port = arg("--port").and_then(|p| p.parse().ok()).unwrap_or(PORT);
    let disc = arg("--disc").and_then(|p| p.parse().ok()).unwrap_or(PORT + 1);
    let disc_send = arg("--disc-send").map(|s| s.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or(vec![disc]);
    let window = !args.iter().any(|a| a == "--no-window");
    let _ = fs::create_dir_all(&data);
    *LOG.lock().unwrap() = fs::OpenOptions::new().create(true).append(true).open(data.join("log.txt")).ok();

    let Ok(listener) = std::net::TcpListener::bind(("0.0.0.0", port)) else {
        // Already running: show its window.
        let ping = net::request("GET", ([127, 0, 0, 1], port).into(), "/api/ping", &[], Duration::from_secs(2)).and_then(|r| r.json());
        if ping.map(|v| v["app"] == net::APP_TAG).unwrap_or(false) {
            if window {
                store::init(data, port, disc, disc_send);
                api::open_window(port);
            }
        } else {
            log!("port {} is taken by another program", port);
        }
        return;
    };
    store::init(data, port, disc, disc_send);
    let a = store::app();
    log!("Githost {} as {} on port {}", env!("CARGO_PKG_VERSION"), a.cfg().device_name, port);
    for fc in a.cfg().folders.clone() {
        a.open_folder(&fc);
    }
    net::start();
    if window {
        api::open_window(port);
    }
    net::serve(listener, api::handle);
}
