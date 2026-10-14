//! Takes changes from the other laptops: each second, every laptop sharing the folder is asked
//! what changed since last time. A version that came after this laptop's is taken; a version
//! changed apart from this one is a conflict, settled the same way on every laptop (a file beats a
//! deletion, then the later edit wins) and the losing side keeps its copy beside the winner.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::folder_path;
use crate::http::{self, Req};
use crate::ignore::{Rules, SCRATCH};
use crate::index::{order, Entry, Order};
use crate::log;
use crate::peers::{self, Peer};
use crate::scan::ignored_path;
use crate::state::{app, Folder};
use crate::util::{hex, hmac_hex, mtime_ms, now_ms, now_s, random_bytes, same, url_encode};

pub fn run(f: Arc<Folder>) {
    while !f.gone() {
        let peers = peers::for_folder(&f.id);
        for p in &peers {
            if let Err(e) = pull(&f, p) {
                log!("{}: from {}: {}", f.name(), p.name, e);
            }
        }
        thread::sleep(Duration::from_millis(1000));
    }
}

pub fn sign(f: &Folder, target: &str) -> Vec<(&'static str, String)> {
    let me = app().me();
    let t = now_s().to_string();
    let mac = hmac_hex(&f.key, format!("fs1|{}|{}|{}", me, t, target).as_bytes());
    vec![("X-FS-Device", me), ("X-FS-Time", t), ("X-FS-Auth", mac)]
}

/// Whether a request was signed by a laptop that knows this folder's code.
pub fn verify(f: &Folder, req: &Req) -> bool {
    let (Some(dev), Some(t), Some(mac)) = (req.header("X-FS-Device"), req.header("X-FS-Time"), req.header("X-FS-Auth")) else {
        return false;
    };
    let Ok(ts) = t.parse::<u64>() else { return false };
    if now_s().abs_diff(ts) > 300 {
        return false;
    }
    same(mac, &hmac_hex(&f.key, format!("fs1|{}|{}|{}", dev, t, req.target).as_bytes()))
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn pull(f: &Folder, p: &Peer) -> Result<(), String> {
    let since = f.idx.lock().unwrap().since.get(&p.id).copied().unwrap_or(0);
    let target = format!("/p/{}/changes?since={}", f.id, since);
    let r = http::request("GET", p.addr, &target, &sign(f, &target), b"", Duration::from_secs(30)).map_err(err)?;
    if r.status != 200 {
        return Err(format!("asked for changes, got {}", r.status));
    }
    let v = r.json().map_err(err)?;
    let seq = v.get("seq").and_then(|x| x.as_u64()).unwrap_or(0);
    if seq < since {
        // Its index started over: take everything again.
        let mut ix = f.idx.lock().unwrap();
        ix.since.insert(p.id.clone(), 0);
        ix.dirty = true;
        return Ok(());
    }
    crate::git::from_peer(f, p, v.get("git"));
    let entries: Vec<(String, Entry)> =
        v.get("entries").and_then(|x| x.as_array()).map(|a| a.iter().filter_map(Entry::from_json).collect()).unwrap_or_default();
    let rules = Rules::load(&f.root);
    let mut upto = seq;
    let mut pending = 0u64;
    let mut last_err = String::new();
    for (path, re) in &entries {
        if !crate::index::safe_path(path) || ignored_path(&rules, path) {
            continue;
        }
        if let Err(e) = step(f, p, path, re) {
            pending += 1;
            last_err = format!("{}: {}", path, e);
            upto = upto.min(re.seq.saturating_sub(1));
        }
    }
    {
        let mut ix = f.idx.lock().unwrap();
        if ix.since.get(&p.id) != Some(&upto) {
            ix.since.insert(p.id.clone(), upto);
            ix.dirty = true;
        }
        ix.save();
    }
    let mut st = f.st.lock().unwrap();
    st.pending = pending;
    st.current.clear();
    if pending == 0 {
        st.synced_with.insert(p.id.clone(), now_ms());
        if !st.error.ends_with("is not there") {
            st.error.clear();
        }
    } else {
        st.synced_with.remove(&p.id);
        st.error = last_err;
    }
    Ok(())
}

enum Act {
    Nothing,
    Record,
    Merge,
    Delete,
    Fetch { conflict: bool },
}

fn decide(l: Option<&Entry>, re: &Entry) -> Act {
    let Some(l) = l else {
        return if re.deleted { Act::Record } else { Act::Fetch { conflict: false } };
    };
    let same_content = (re.deleted && l.deleted) || (!re.deleted && !l.deleted && re.hash == l.hash);
    match order(&re.vv, &l.vv) {
        Order::Equal | Order::Before => Act::Nothing,
        Order::After if same_content => Act::Merge,
        Order::After if re.deleted => Act::Delete,
        Order::After => Act::Fetch { conflict: false },
        Order::Apart if same_content => Act::Merge,
        Order::Apart => {
            let remote_wins = if re.deleted != l.deleted { !re.deleted } else { (re.mtime, &re.by) > (l.mtime, &l.by) };
            match (remote_wins, l.deleted) {
                (false, _) => Act::Nothing, // the other laptop keeps its copy aside when it takes this one
                (true, true) => Act::Fetch { conflict: false },
                (true, false) => Act::Fetch { conflict: true },
            }
        }
    }
}

/// Whether the file on disk is still what the index says (no edit here that is not yet noticed).
fn unchanged_here(full: &Path, l: Option<&Entry>) -> bool {
    match (fs::metadata(full), l) {
        (Ok(md), Some(e)) if !e.deleted => md.len() == e.size && mtime_ms(&md) == e.mtime,
        (Ok(_), _) => false,
        (Err(_), Some(e)) => e.deleted,
        (Err(_), None) => true,
    }
}

fn step(f: &Folder, p: &Peer, path: &str, re: &Entry) -> Result<(), String> {
    let full = folder_path(&f.root, path);
    let (act, l) = {
        let mut ix = f.idx.lock().unwrap();
        let l = ix.entries.get(path).cloned();
        let act = decide(l.as_ref(), re);
        match act {
            Act::Nothing => return Ok(()),
            Act::Record => {
                ix.take_remote(path, re, 0);
                return Ok(());
            }
            Act::Merge => {
                ix.merge_vv(path, &re.vv);
                return Ok(());
            }
            _ => {}
        }
        (act, l)
    };
    if !unchanged_here(&full, l.as_ref()) {
        return Err("changed here too; looking again".into());
    }
    match act {
        Act::Delete => {
            let mut ix = f.idx.lock().unwrap();
            if ix.entries.get(path).map(|e| e.seq) != l.as_ref().map(|e| e.seq) || !unchanged_here(&full, l.as_ref()) {
                return Err("changed here too; looking again".into());
            }
            match fs::remove_file(&full) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("cannot delete: {}", e)),
            }
            remove_empty_parents(&f.root, &full);
            ix.take_remote(path, re, 0);
            drop(ix);
            f.note("deleted", path, &re.by);
            Ok(())
        }
        Act::Fetch { conflict } => {
            f.st.lock().unwrap().current = path.to_string();
            let tmp = fetch(f, p, path, re)?;
            let mut ix = f.idx.lock().unwrap();
            if ix.entries.get(path).map(|e| e.seq) != l.as_ref().map(|e| e.seq) || !unchanged_here(&full, l.as_ref()) {
                let _ = fs::remove_file(&tmp);
                return Err("changed here too; looking again".into());
            }
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).map_err(err)?;
            }
            if conflict {
                let aside = conflict_name(&full, &app().cfg().device_name);
                fs::rename(&full, &aside).map_err(|e| format!("cannot keep this laptop's copy: {}", e))?;
            }
            if let Err(e) = fs::rename(&tmp, &full) {
                let _ = fs::remove_file(&tmp);
                return Err(format!("cannot write (open in another program?): {}", e));
            }
            let mt = fs::metadata(&full).map(|m| mtime_ms(&m)).unwrap_or(re.mtime);
            ix.take_remote(path, re, mt);
            drop(ix);
            f.note(if conflict { "conflict" } else { "got" }, path, &re.by);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Gets the file's content into the folder's scratch space: from a file here with the same
/// content when there is one (a move or a copy), otherwise from the other laptop.
fn fetch(f: &Folder, p: &Peer, path: &str, re: &Entry) -> Result<PathBuf, String> {
    let scratch = f.root.join(SCRATCH);
    fs::create_dir_all(&scratch).map_err(err)?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let w: Vec<u16> = scratch.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe { SetFileAttributesW(w.as_ptr(), 0x2) }; // hidden
    }
    let tmp = scratch.join(format!("{}.part", hex(&random_bytes(8))));
    let local = {
        let ix = f.idx.lock().unwrap();
        ix.entries
            .iter()
            .find(|(q, e)| !e.deleted && e.hash == re.hash && q.as_str() != path && unchanged_here(&folder_path(&f.root, q), Some(e)))
            .map(|(q, _)| folder_path(&f.root, q))
    };
    let got = match local {
        Some(src) => copy_hashing(&mut File::open(&src).map_err(err)?, &tmp, None),
        None => {
            let target = format!("/p/{}/file?path={}&hash={}", f.id, url_encode(path), re.hash);
            let r = http::request("GET", p.addr, &target, &sign(f, &target), b"", Duration::from_secs(60)).map_err(err)?;
            if r.status != 200 {
                return Err(format!("{} does not have this version now ({})", p.name, r.status));
            }
            let len = r.len;
            let mut body = r.body;
            copy_hashing(&mut body, &tmp, len)
        }
    };
    match got {
        Ok(h) if h == re.hash => {}
        Ok(_) => {
            let _ = fs::remove_file(&tmp);
            return Err("changed while it was sent; again".into());
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(e.to_string());
        }
    }
    if let Ok(file) = File::options().write(true).open(&tmp) {
        let _ = file.set_modified(UNIX_EPOCH + Duration::from_millis(re.mtime.max(0) as u64));
    }
    Ok(tmp)
}

fn copy_hashing(src: &mut dyn Read, dst: &Path, len: Option<u64>) -> io::Result<String> {
    let mut out = File::create(dst)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut left = len.unwrap_or(u64::MAX);
    while left > 0 {
        let want = buf.len().min(left.min(usize::MAX as u64) as usize);
        let n = src.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        left -= n as u64;
    }
    if len.is_some() && left > 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "cut off"));
    }
    out.sync_all()?;
    Ok(hex(&h.finalize()))
}

fn conflict_name(full: &Path, me: &str) -> PathBuf {
    let stem = full.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = full.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let who: String = me.chars().filter(|c| !"\\/:*?\"<>|".contains(*c)).collect();
    let dir = full.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut n = 1;
    loop {
        let extra = if n == 1 { String::new() } else { format!(" {}", n) };
        let c = dir.join(format!("{} ({}'s copy{}){}", stem, who, extra, ext));
        if !c.exists() {
            return c;
        }
        n += 1;
    }
}

fn remove_empty_parents(root: &Path, full: &Path) {
    let mut d = full.parent();
    while let Some(dir) = d {
        if dir == root || !dir.starts_with(root) || fs::remove_dir(dir).is_err() {
            break;
        }
        d = dir.parent();
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn SetFileAttributesW(name: *const u16, attrs: u32) -> i32;
}

