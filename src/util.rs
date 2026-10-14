//! Small things everything uses: time, hex, hashing, signing, the log, and starting programs
//! without a console window flashing up.

use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn now_s() -> u64 {
    now_ms() / 1000
}

pub fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    getrandom::getrandom(&mut v).expect("no randomness");
    v
}

pub fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

pub fn hash_file(p: &Path) -> io::Result<String> {
    let mut f = File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

pub fn hmac_hex(key: &[u8], msg: &[u8]) -> String {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac key");
    m.update(msg);
    hex(&m.finalize().into_bytes())
}

/// Compares two strings in time that does not depend on where they differ.
pub fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn mtime_ms(md: &Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()).ok_or(()) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || b"-_.~/".contains(&c) {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{:02X}", c));
        }
    }
    out
}

/// A command that never opens a console window of its own on Windows.
pub fn quiet_command(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

static LOG: OnceLock<Mutex<Option<File>>> = OnceLock::new();

pub fn open_log(p: &Path) {
    let f = OpenOptions::new().create(true).append(true).open(p).ok();
    let _ = LOG.set(Mutex::new(f));
}

pub fn log_line(s: &str) {
    let line = format!("{} {}\n", now_ms(), s);
    eprint!("{}", line);
    if let Some(m) = LOG.get() {
        if let Ok(mut g) = m.lock() {
            if let Some(f) = g.as_mut() {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::util::log_line(&format!($($t)*)) };
}
