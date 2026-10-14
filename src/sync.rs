//! Keeping a folder the same everywhere. Every second the folder is walked: a file whose size or time
//! differs from the index is hashed, and a new hash is a new version; a file gone is marked deleted.
//! Then every laptop sharing the folder is asked what changed since last time. A version that came
//! after this laptop's is taken; one changed apart from this one is a conflict, settled the same way
//! on every laptop (a file beats a deletion, then the later edit wins), and the losing side keeps its
//! copy beside the winner. The folder's .gitignore and .syncignore say what stays on each laptop.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use crate::net::{self, Peer, Req};
use crate::store::{app, order, safe_path, Entry, Folder, Order};
use crate::{folder_path, hash_copy, hash_file, hex, hmac, log, mtime, now_ms, random, url_encode};

pub const SCRATCH: &str = ".foldersync-tmp";
const ALWAYS: &[&str] = &[".git", SCRATCH, ".DS_Store", "Thumbs.db", "desktop.ini", "~$*", ".~lock.*#"];

/// One .gitignore line: the pattern, whether it holds from the top only, for folders only, and whether it is a "!" one.
type Rule = (String, bool, bool, bool);

pub fn rules(root: &Path) -> Vec<Rule> {
    let text = [".gitignore", ".syncignore"].map(|n| fs::read_to_string(root.join(n)).unwrap_or_default()).join("\n");
    text.lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (negate, l) = l.strip_prefix('!').map_or((false, l), |r| (true, r));
            let (dir_only, l) = (l.ends_with('/'), l.trim_end_matches('/'));
            (l.trim_start_matches('/').to_string(), l.contains('/'), dir_only, negate)
        })
        .filter(|r| !r.0.is_empty())
        .collect()
}

pub fn ignored(rules: &[Rule], rel: &str, is_dir: bool) -> bool {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    if ALWAYS.iter().any(|p| glob(p.as_bytes(), base.as_bytes())) {
        return true;
    }
    let hit = |(p, anchored, dir_only, _): &&Rule| (is_dir || !dir_only) && glob(p.as_bytes(), (if *anchored { rel } else { base }).as_bytes());
    rules.iter().filter(hit).last().is_some_and(|r| !r.3)
}

/// Whether a path, or any folder it is in, stays on each laptop.
pub fn ignored_path(rules: &[Rule], p: &str) -> bool {
    p.match_indices('/').any(|(i, _)| ignored(rules, &p[..i], true)) || ignored(rules, p, false)
}

/// A .gitignore pattern: * within a part, ** across parts, ?, and [classes].
pub fn glob(p: &[u8], s: &[u8]) -> bool {
    let eq = |a: u8, b: u8| a.eq_ignore_ascii_case(&b);
    match p {
        [] => s.is_empty(),
        [b'*', b'*', b'/', rest @ ..] => glob(rest, s) || (0..s.len()).any(|i| s[i] == b'/' && glob(rest, &s[i + 1..])),
        [b'*', b'*', rest @ ..] => (0..=s.len()).any(|i| glob(rest, &s[i..])),
        [b'*', rest @ ..] => (0..=s.len()).take_while(|&i| i == 0 || s[i - 1] != b'/').any(|i| glob(rest, &s[i..])),
        [b'?', rest @ ..] => !s.is_empty() && s[0] != b'/' && glob(rest, &s[1..]),
        [b'[', rest @ ..] if rest.iter().skip(1).any(|&c| c == b']') => {
            let end = rest.iter().skip(1).position(|&c| c == b']').unwrap() + 1;
            let (neg, class) = match rest[0] {
                b'!' | b'^' => (true, &rest[1..end]),
                _ => (false, &rest[..end]),
            };
            let Some(&c) = s.first() else { return false };
            let (cl, mut i, mut hit) = (c.to_ascii_lowercase(), 0, false);
            while i < class.len() {
                if i + 2 < class.len() && class[i + 1] == b'-' {
                    hit |= class[i].to_ascii_lowercase() <= cl && cl <= class[i + 2].to_ascii_lowercase();
                    i += 3;
                } else {
                    hit |= eq(class[i], c);
                    i += 1;
                }
            }
            hit != neg && c != b'/' && glob(&rest[end + 1..], &s[1..])
        }
        [b'\\', c, rest @ ..] | [c, rest @ ..] => !s.is_empty() && eq(s[0], *c) && glob(rest, &s[1..]),
    }
}

pub fn run(f: Arc<Folder>) {
    while !f.gone() {
        scan(&f);
        for p in net::for_folder(&f.id) {
            if let Err(e) = pull(&f, &p) {
                log!("{}: from {}: {}", f.name(), p.name, e);
            }
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
}

fn walk(root: &Path, rel: &str, rules: &[Rule], files: &mut Vec<(String, u64, i64)>, unreadable: &mut Vec<String>) {
    let Ok(rd) = fs::read_dir(folder_path(root, rel)) else { return unreadable.push(rel.to_string()) };
    for e in rd.flatten() {
        let (Ok(ft), Ok(md)) = (e.file_type(), e.metadata()) else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        let p = if rel.is_empty() { name } else { format!("{}/{}", rel, name) };
        if ft.is_symlink() || ignored(rules, &p, ft.is_dir()) {
            continue;
        }
        if ft.is_dir() {
            walk(root, &p, rules, files, unreadable);
        } else if ft.is_file() {
            files.push((p, md.len(), mtime(&md)));
        }
    }
}

/// Notices changes made on this laptop.
pub fn scan(f: &Folder) {
    if !f.root.is_dir() {
        f.st.lock().unwrap().error = format!("{} is not there", f.root.display());
        return;
    }
    let rules = rules(&f.root);
    let (mut files, mut unreadable) = (Vec::new(), Vec::new());
    walk(&f.root, "", &rules, &mut files, &mut unreadable);
    let me = app().me();
    // The files that look changed are hashed outside the lock, noted with the entry as it was.
    let todo: Vec<_> = {
        let ix = f.idx.lock().unwrap();
        files.iter().filter(|(p, size, mt)| !ix.entries.get(p).is_some_and(|e| !e.deleted && e.size == *size && e.mtime == *mt)).map(|(p, size, mt)| (p.clone(), *size, *mt, ix.entries.get(p).map(|e| e.seq))).collect()
    };
    let hashed: Vec<_> = todo.into_iter().filter_map(|(p, size, mt, seq)| Some((hash_file(&folder_path(&f.root, &p)).ok()?, p, size, mt, seq))).collect();
    let mut ix = f.idx.lock().unwrap();
    for (h, p, size, mt, seq) in hashed {
        let still = fs::metadata(folder_path(&f.root, &p)).is_ok_and(|md| md.len() == size && mtime(&md) == mt);
        if ix.entries.get(&p).map(|e| e.seq) != seq || !still {
            continue; // taken from another laptop while being hashed, or still being written
        }
        match ix.entries.get_mut(&p) {
            Some(e) if !e.deleted && e.hash == h => e.mtime = mt,
            _ => {
                ix.local_change(&me, &p, size, mt, h, false);
                f.note("changed", &p, &me);
            }
        }
        ix.dirty = true;
    }
    // A folder that suddenly looks empty (a drive unplugged) is taken as emptied only when it stays so.
    let live = ix.entries.values().filter(|e| !e.deleted).count();
    {
        let mut st = f.st.lock().unwrap();
        st.empty_scans = if files.is_empty() && live > 0 { st.empty_scans + 1 } else { 0 };
        if (1..4).contains(&st.empty_scans) {
            return;
        }
        if st.error.ends_with("is not there") {
            st.error.clear();
        }
    }
    let seen: HashSet<&str> = files.iter().map(|(p, _, _)| p.as_str()).collect();
    let missing: Vec<String> = ix
        .entries
        .iter()
        .filter(|(p, e)| !e.deleted && !seen.contains(p.as_str()) && !unreadable.iter().any(|u| u.is_empty() || p.starts_with(&format!("{}/", u))))
        .map(|(p, _)| p.clone())
        .collect();
    for p in missing {
        if !folder_path(&f.root, &p).exists() && !ignored_path(&rules, &p) {
            ix.local_change(&me, &p, 0, 0, String::new(), true);
            f.note("deleted", &p, &me);
        }
    }
    f.save(&mut ix);
}

pub fn sign(f: &Folder, target: &str) -> Vec<(&'static str, String)> {
    let (me, t) = (app().me(), (now_ms() / 1000).to_string());
    let mac = hmac(&f.key, &format!("fs1|{}|{}|{}", me, t, target));
    vec![("X-FS-Device", me), ("X-FS-Time", t), ("X-FS-Auth", mac)]
}

/// Whether a request was signed by a laptop that has this folder's key.
pub fn verify(f: &Folder, req: &Req) -> bool {
    let (dev, t, mac) = (req.header("X-FS-Device"), req.header("X-FS-Time"), req.header("X-FS-Auth"));
    let fresh = t.parse::<u64>().is_ok_and(|ts| (now_ms() / 1000).abs_diff(ts) < 300);
    let want = hmac(&f.key, &format!("fs1|{}|{}|{}", dev, t, req.target));
    fresh && crate::same(mac, &want)
}

fn pull(f: &Folder, p: &Peer) -> Result<(), String> {
    let since = f.idx.lock().unwrap().since.get(&p.id).copied().unwrap_or(0);
    let target = format!("/p/{}/changes?since={}", f.id, since);
    let v = net::request("GET", p.addr, &target, &sign(f, &target), Duration::from_secs(30)).and_then(net::Reply::json).map_err(|e| e.to_string())?;
    let seq = v["seq"].as_u64().ok_or("no changes in the answer")?;
    let mut upto = if seq < since { 0 } else { seq }; // below: its index started over, so take everything again
    crate::git::from_peer(f, p, &v["git"]);
    let entries: Vec<(String, Entry)> = serde_json::from_value(v["entries"].clone()).unwrap_or_default();
    let rules = rules(&f.root);
    let (mut pending, mut last_err) = (0, String::new());
    for (path, re) in entries.iter().filter(|(p, _)| seq >= since && safe_path(p) && !ignored_path(&rules, p)) {
        if let Err(e) = step(f, p, path, re) {
            (pending, last_err, upto) = (pending + 1, format!("{}: {}", path, e), upto.min(re.seq.saturating_sub(1)));
        }
    }
    let mut ix = f.idx.lock().unwrap();
    if ix.since.insert(p.id.clone(), upto) != Some(upto) {
        ix.dirty = true;
    }
    f.save(&mut ix);
    let mut st = f.st.lock().unwrap();
    (st.pending, st.current) = (pending, String::new());
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
    Fetch(bool),
}

fn decide(l: Option<&Entry>, re: &Entry) -> Act {
    let Some(l) = l else { return if re.deleted { Act::Record } else { Act::Fetch(false) } };
    let same = (re.deleted && l.deleted) || (!re.deleted && !l.deleted && re.hash == l.hash);
    match order(&re.vv, &l.vv) {
        Order::Equal | Order::Before => Act::Nothing,
        Order::After | Order::Apart if same => Act::Merge,
        Order::After if re.deleted => Act::Delete,
        Order::After => Act::Fetch(false),
        // Changed apart: the other laptop keeps its own copy aside when this one wins.
        Order::Apart if re.deleted != l.deleted => if re.deleted { Act::Nothing } else { Act::Fetch(false) },
        Order::Apart => if (re.mtime, &re.by) > (l.mtime, &l.by) { Act::Fetch(true) } else { Act::Nothing },
    }
}

/// Whether the file on disk is still what the index says (no edit here that is not yet noticed).
fn unchanged_here(full: &Path, l: Option<&Entry>) -> bool {
    match (fs::metadata(full), l) {
        (Ok(md), Some(e)) => !e.deleted && md.len() == e.size && mtime(&md) == e.mtime,
        (Ok(_), None) => false,
        (Err(_), l) => l.map_or(true, |e| e.deleted),
    }
}

fn step(f: &Folder, p: &Peer, path: &str, re: &Entry) -> Result<(), String> {
    let full = folder_path(&f.root, path);
    let l = f.idx.lock().unwrap().entries.get(path).cloned();
    let act = decide(l.as_ref(), re);
    let tmp = match act {
        Act::Nothing => return Ok(()),
        Act::Fetch(_) => Some(fetch(f, p, path, re)?),
        _ => None,
    };
    let mut ix = f.idx.lock().unwrap();
    let busy = "changed here too; looking again";
    if ix.entries.get(path).map(|e| e.seq) != l.as_ref().map(|e| e.seq) || (matches!(act, Act::Delete | Act::Fetch(_)) && !unchanged_here(&full, l.as_ref())) {
        let _ = tmp.map(fs::remove_file);
        return Err(busy.into());
    }
    match act {
        Act::Record => ix.take_remote(path, re, 0),
        Act::Merge => ix.merge_vv(path, &re.vv),
        Act::Delete => {
            if let Err(e) = fs::remove_file(&full).or_else(|e| if e.kind() == io::ErrorKind::NotFound { Ok(()) } else { Err(e) }) {
                return Err(format!("cannot delete: {}", e));
            }
            // Folders left empty go too.
            full.ancestors().skip(1).take_while(|d| *d != f.root && d.starts_with(&f.root)).take_while(|d| fs::remove_dir(d).is_ok()).count();
            ix.take_remote(path, re, 0);
            f.note("deleted", path, &re.by);
        }
        Act::Fetch(conflict) => {
            let tmp = tmp.unwrap();
            let put = || -> io::Result<()> {
                fs::create_dir_all(full.parent().unwrap())?;
                if conflict {
                    fs::rename(&full, conflict_name(&full, &app().cfg().device_name))?;
                }
                fs::rename(&tmp, &full)
            };
            if let Err(e) = put() {
                let _ = fs::remove_file(&tmp);
                return Err(format!("cannot write (open in another program?): {}", e));
            }
            let mt = fs::metadata(&full).map_or(re.mtime, |m| mtime(&m));
            ix.take_remote(path, re, mt);
            f.note(if conflict { "conflict" } else { "got" }, path, &re.by);
        }
        Act::Nothing => {}
    }
    Ok(())
}

/// Gets the file into the folder's scratch space: from a file here with the same content when there
/// is one (a move or a copy), otherwise from the other laptop. Its hash is checked either way.
fn fetch(f: &Folder, p: &Peer, path: &str, re: &Entry) -> Result<PathBuf, String> {
    let scratch = f.root.join(SCRATCH);
    if !scratch.exists() {
        fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
        #[cfg(windows)]
        let _ = crate::quiet("attrib").arg("+h").arg(&scratch).status();
    }
    f.st.lock().unwrap().current = path.to_string();
    let tmp = scratch.join(format!("{}.part", hex(&random(8))));
    let local = f.idx.lock().unwrap().entries.iter().find(|(q, e)| !e.deleted && e.hash == re.hash && *q != path && unchanged_here(&folder_path(&f.root, q), Some(e))).map(|(q, _)| folder_path(&f.root, q));
    let got = File::create(&tmp).and_then(|mut out| match &local {
        Some(src) => hash_copy(&mut File::open(src)?, Some(&mut out), None),
        None => {
            let target = format!("/p/{}/file?path={}&hash={}", f.id, url_encode(path), re.hash);
            let r = net::request("GET", p.addr, &target, &sign(f, &target), Duration::from_secs(60))?;
            if r.status != 200 {
                return Err(io::Error::other(format!("{} does not have this version now", p.name)));
            }
            hash_copy(&mut { r.body }, Some(&mut out), r.len)
        }
    });
    let res = match got {
        Ok(h) if h == re.hash => File::options().write(true).open(&tmp).and_then(|t| t.set_modified(UNIX_EPOCH + Duration::from_millis(re.mtime.max(0) as u64))).map_err(|e| e.to_string()),
        Ok(_) => Err("changed while it was sent; again".into()),
        Err(e) => Err(e.to_string()),
    };
    res.map(|_| tmp.clone()).inspect_err(|_| drop(fs::remove_file(&tmp)))
}

fn conflict_name(full: &Path, me: &str) -> PathBuf {
    let stem = full.file_stem().unwrap_or_default().to_string_lossy();
    let ext = full.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let who: String = me.chars().filter(|c| !"\\/:*?\"<>|".contains(*c)).collect();
    (1..).map(|n| full.with_file_name(format!("{} ({}'s copy{}){}", stem, who, if n == 1 { String::new() } else { format!(" {}", n) }, ext))).find(|c| !c.exists()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Vv;

    #[test]
    fn globs_and_rules() {
        for (p, s, want) in [
            ("*.log", "a.log", true),
            ("*.log", "a/b.log", false),
            ("**/x", "a/b/x", true),
            ("**/x", "x", true),
            ("a/**", "a/b/c", true),
            ("a/**/c", "a/c", true),
            ("a/**/c", "a/b/d/c", true),
            ("[ab].txt", "b.txt", true),
            ("[!ab].txt", "b.txt", false),
            ("[a-c]x", "Bx", true),
            ("file?.txt", "file1.txt", true),
            ("~$*", "~$Doc.docx", true),
        ] {
            assert_eq!(glob(p.as_bytes(), s.as_bytes()), want, "{} {}", p, s);
        }
        let dir = std::env::temp_dir().join(format!("fsync-ign-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".gitignore"), "node_modules/\n*.log\n!keep.log\n/build\ndocs/*.tmp\n").unwrap();
        let r = rules(&dir);
        assert!(ignored(&r, "node_modules", true) && !ignored(&r, "node_modules", false));
        assert!(ignored(&r, "x/y.log", false) && !ignored(&r, "keep.log", false));
        assert!(ignored(&r, "build", true) && !ignored(&r, "src/build", true));
        assert!(ignored(&r, "docs/a.tmp", false) && ignored(&r, ".git", true) && ignored(&r, "a/Thumbs.db", false));
        assert!(ignored_path(&r, "node_modules/x/a.js") && !ignored_path(&r, "src/main.rs"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn orders_and_paths() {
        let vv = |p: &[(&str, u64)]| p.iter().map(|(k, v)| (k.to_string(), *v)).collect::<Vv>();
        assert_eq!(order(&vv(&[("a", 1)]), &vv(&[("a", 1)])), Order::Equal);
        assert_eq!(order(&vv(&[("a", 2)]), &vv(&[("a", 1)])), Order::After);
        assert_eq!(order(&vv(&[("a", 1)]), &vv(&[("a", 1), ("b", 1)])), Order::Before);
        assert_eq!(order(&vv(&[("a", 2)]), &vv(&[("a", 1), ("b", 1)])), Order::Apart);
        assert!(safe_path("a/b.txt"));
        for bad in ["../x", "a/../../x", "/etc/passwd", "C:/x", "a\\b", "a//b"] {
            assert!(!safe_path(bad), "{}", bad);
        }
    }
}
