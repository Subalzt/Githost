//! Keeping a folder the same everywhere. Every second the folder is walked: a file whose size or time
//! differs from the index is hashed, and a new hash is a new version; a file gone is marked deleted.
//! Folders (empty ones too) and symbolic links are entries as well, with "dir" or "link:<target>" for
//! a hash. Then every laptop sharing the folder is asked what changed since last time. A version that
//! came after this laptop's is taken; one changed apart from this one is a conflict, settled the same
//! way on every laptop (a file beats a deletion, then the later edit wins), and the losing side keeps
//! its copy beside the winner. The .gitignore and .syncignore files, at the top and in any folder
//! below it, say what stays on each laptop, as they do for git.

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

/// One .gitignore line, with the folder its file is in: it holds only below that folder.
#[derive(Clone, Debug)]
pub struct Rule {
    base: String,
    pat: String,
    anchored: bool,
    dir_only: bool,
    negate: bool,
}

/// Adds the rules of the .gitignore and .syncignore in `dir` ("" for the top). Rules found deeper
/// come later, and the last rule that matches decides, so a deeper file overrides a higher one.
pub fn add_rules(root: &Path, dir: &str, rules: &mut Vec<Rule>) {
    for name in [".gitignore", ".syncignore"] {
        let Ok(text) = fs::read_to_string(folder_path(root, dir).join(name)) else { continue };
        for l in text.lines().map(str::trim_end).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let (negate, l) = l.strip_prefix('!').map_or((false, l), |r| (true, r));
            let (dir_only, l) = (l.ends_with('/'), l.trim_end_matches('/'));
            let pat = l.trim_start_matches('/').to_string();
            if !pat.is_empty() {
                rules.push(Rule { base: dir.to_string(), pat, anchored: l.contains('/'), dir_only, negate });
            }
        }
    }
}

pub fn ignored(rules: &[Rule], rel: &str, is_dir: bool) -> bool {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    if ALWAYS.iter().any(|p| glob(p.as_bytes(), base.as_bytes())) {
        return true;
    }
    let hit = |r: &&Rule| {
        let below = if r.base.is_empty() { Some(rel) } else { rel.strip_prefix(r.base.as_str()).and_then(|s| s.strip_prefix('/')) };
        (is_dir || !r.dir_only) && below.is_some_and(|below| glob(r.pat.as_bytes(), (if r.anchored { below } else { base }).as_bytes()))
    };
    rules.iter().filter(hit).last().is_some_and(|r| !r.negate)
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

/// Where a link points, written with / so laptops on other systems read it the same way. A link to
/// somewhere in the folder written as a full path (as Windows junctions are) is made relative, so it
/// points to the same place on every laptop.
fn link_target(root: &Path, full: &Path) -> Option<String> {
    let t = fs::read_link(full).ok()?;
    let t = PathBuf::from(t.to_string_lossy().trim_start_matches(r"\\?\").trim_start_matches(r"\??\"));
    let t = match (t.is_absolute(), t.strip_prefix(root), full.parent().and_then(|p| p.strip_prefix(root).ok())) {
        (true, Ok(inside), Some(from)) => PathBuf::from("../".repeat(from.components().count())).join(inside),
        _ => t,
    };
    Some(t.to_string_lossy().replace('\\', "/"))
}

/// What is at a path here, as the index writes it: (size, time, hash), the hash left out for a file.
struct Item {
    path: String,
    size: u64,
    mtime: i64,
    hash: Option<String>,
}

fn walk(root: &Path, rel: &str, rules: &mut Vec<Rule>, items: &mut Vec<Item>, unreadable: &mut Vec<String>) {
    add_rules(root, rel, rules);
    let Ok(rd) = fs::read_dir(folder_path(root, rel)) else { return unreadable.push(rel.to_string()) };
    for e in rd.flatten() {
        let Ok(md) = fs::symlink_metadata(e.path()) else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        let p = if rel.is_empty() { name } else { format!("{}/{}", rel, name) };
        let ft = md.file_type();
        if ignored(rules, &p, ft.is_dir()) {
            continue;
        }
        if ft.is_symlink() {
            // Not followed: the link itself travels.
            let Some(t) = link_target(root, &e.path()) else { continue };
            items.push(Item { path: p, size: 0, mtime: 0, hash: Some(format!("link:{}", t)) });
        } else if ft.is_dir() {
            items.push(Item { path: p.clone(), size: 0, mtime: 0, hash: Some("dir".into()) });
            walk(root, &p, rules, items, unreadable);
        } else if ft.is_file() {
            items.push(Item { path: p, size: md.len(), mtime: mtime(&md), hash: None });
        }
    }
}

/// Notices changes made on this laptop.
pub fn scan(f: &Folder) {
    if !f.root.is_dir() {
        f.st.lock().unwrap().error = format!("{} is not there", f.root.display());
        return;
    }
    let (mut rules, mut items, mut unreadable) = (Vec::new(), Vec::new(), Vec::new());
    walk(&f.root, "", &mut rules, &mut items, &mut unreadable);
    *f.rules.lock().unwrap() = rules.clone();
    let me = app().me();
    // What looks changed, noted with the entry as it was; files are hashed outside the lock.
    let todo: Vec<_> = {
        let ix = f.idx.lock().unwrap();
        let same = |i: &Item, e: &Entry| !e.deleted && i.size == e.size && i.mtime == e.mtime && i.hash.as_ref().map_or(e.is_file(), |h| *h == e.hash);
        items.iter().filter(|i| !ix.entries.get(&i.path).is_some_and(|e| same(i, e))).map(|i| (i, ix.entries.get(&i.path).map(|e| e.seq))).collect()
    };
    let hashed: Vec<_> = todo.into_iter().filter_map(|(i, seq)| Some((i.hash.clone().or_else(|| hash_file(&folder_path(&f.root, &i.path)).ok())?, i, seq))).collect();
    let mut ix = f.idx.lock().unwrap();
    for (h, i, seq) in hashed {
        let still = i.hash.is_some() || fs::metadata(folder_path(&f.root, &i.path)).is_ok_and(|md| md.len() == i.size && mtime(&md) == i.mtime);
        if ix.entries.get(&i.path).map(|e| e.seq) != seq || !still {
            continue; // taken from another laptop while being hashed, or still being written
        }
        match ix.entries.get_mut(&i.path) {
            Some(e) if !e.deleted && e.hash == h => e.mtime = i.mtime,
            _ => {
                let is_dir = h == "dir";
                ix.local_change(&me, &i.path, i.size, i.mtime, h, false);
                if !is_dir {
                    f.note("changed", &i.path, &me);
                }
            }
        }
        ix.dirty = true;
    }
    // A folder that suddenly looks empty (a drive unplugged) is taken as emptied only when it stays so.
    let live = ix.entries.values().filter(|e| !e.deleted).count();
    {
        let mut st = f.st.lock().unwrap();
        st.empty_scans = if items.is_empty() && live > 0 { st.empty_scans + 1 } else { 0 };
        if (1..4).contains(&st.empty_scans) {
            return;
        }
        if st.error.ends_with("is not there") {
            st.error.clear();
        }
    }
    let seen: HashSet<&str> = items.iter().map(|i| i.path.as_str()).collect();
    let missing: Vec<String> = ix
        .entries
        .iter()
        .filter(|(p, e)| !e.deleted && !seen.contains(p.as_str()) && !unreadable.iter().any(|u| u.is_empty() || p.starts_with(&format!("{}/", u))))
        .map(|(p, _)| p.clone())
        .collect();
    // Deepest first, so a folder's contents are gone on the others before the folder is.
    for p in missing.iter().rev() {
        if fs::symlink_metadata(folder_path(&f.root, p)).is_err() && !ignored_path(&rules, p) {
            let was_dir = ix.entries.get(p).is_some_and(|e| e.hash == "dir");
            ix.local_change(&me, p, 0, 0, String::new(), true);
            if !was_dir {
                f.note("deleted", p, &me);
            }
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
    let rules = f.rules.lock().unwrap().clone();
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

/// Whether what is on disk is still what the index says (no change here that is not yet noticed).
fn unchanged_here(root: &Path, full: &Path, l: Option<&Entry>) -> bool {
    let md = fs::symlink_metadata(full);
    match (md, l) {
        (Ok(md), Some(e)) if !e.deleted => match e.hash.strip_prefix("link:") {
            _ if e.hash == "dir" => md.is_dir(),
            Some(t) => md.file_type().is_symlink() && link_target(root, full).as_deref() == Some(t),
            None => md.is_file() && md.len() == e.size && mtime(&md) == e.mtime,
        },
        (md, l) => md.is_err() && l.map_or(true, |e| e.deleted),
    }
}

/// A path whose folder, or a folder above it, is a link here: writing there could land outside.
fn through_link(root: &Path, rel: &str) -> bool {
    rel.match_indices('/').any(|(i, _)| fs::symlink_metadata(folder_path(root, &rel[..i])).is_ok_and(|m| m.file_type().is_symlink()))
}

fn make_link(target: &str, at: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        // Windows has file links and folder links; a link to nothing yet is made a file link.
        let dest = at.parent().and_then(|d| d.join(target).canonicalize().ok()).filter(|t| t.is_dir());
        let target = target.replace('/', "\\");
        let r = if dest.is_some() { std::os::windows::fs::symlink_dir(&target, at) } else { std::os::windows::fs::symlink_file(&target, at) };
        match (r, dest) {
            // Without Developer Mode a folder link can still be a junction, which takes the full path.
            (Err(e), Some(dest)) if e.raw_os_error() == Some(1314) => {
                let dest = dest.to_string_lossy().trim_start_matches(r"\\?\").to_string();
                let made = crate::quiet("cmd").args(["/C", "mklink", "/J"]).arg(at).arg(dest).stdout(std::process::Stdio::null()).status();
                if made.is_ok_and(|s| s.success()) { Ok(()) } else { Err(io::Error::other("cannot make the folder link here")) }
            }
            (Err(e), _) if e.raw_os_error() == Some(1314) => Err(io::Error::other("Windows makes links to files only in Developer Mode (Settings, System, For developers)")),
            (r, _) => r,
        }
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, at)
}

/// Removes a file, a link or an empty folder; a folder with something in it here stays.
fn remove(full: &Path, e: &Entry) -> io::Result<()> {
    let r = if e.hash == "dir" { fs::remove_dir(full) } else { fs::remove_file(full).or_else(|_| fs::remove_dir(full)) };
    r.or_else(|err| if err.kind() == io::ErrorKind::NotFound { Ok(()) } else { Err(err) })
}

fn step(f: &Folder, p: &Peer, path: &str, re: &Entry) -> Result<(), String> {
    let full = folder_path(&f.root, path);
    if through_link(&f.root, path) {
        log!("{}: {} is inside a link here; left alone", f.name(), path);
        return Ok(());
    }
    let l = f.idx.lock().unwrap().entries.get(path).cloned();
    let act = decide(l.as_ref(), re);
    let tmp = match act {
        Act::Nothing => return Ok(()),
        Act::Fetch(_) if re.is_file() => Some(fetch(f, p, path, re)?),
        _ => None,
    };
    let mut ix = f.idx.lock().unwrap();
    let busy = "changed here too; looking again";
    if ix.entries.get(path).map(|e| e.seq) != l.as_ref().map(|e| e.seq) || (matches!(act, Act::Delete | Act::Fetch(_)) && !unchanged_here(&f.root, &full, l.as_ref())) {
        let _ = tmp.map(fs::remove_file);
        return Err(busy.into());
    }
    match act {
        Act::Record => ix.take_remote(path, re, 0),
        Act::Merge => ix.merge_vv(path, &re.vv),
        Act::Delete => match remove(&full, l.as_ref().unwrap()) {
            Ok(()) => {
                ix.take_remote(path, re, 0);
                if l.as_ref().is_some_and(|e| e.hash != "dir") {
                    f.note("deleted", path, &re.by);
                }
            }
            // A folder that still has things in it here: they are newer than the deletion, so it is
            // kept, and said again so the others keep (or bring back) it too.
            Err(_) if re.deleted && l.as_ref().is_some_and(|e| e.hash == "dir") && full.is_dir() => {
                let me = app().me();
                ix.merge_vv(path, &re.vv);
                ix.local_change(&me, path, 0, 0, "dir".into(), false);
            }
            Err(e) => return Err(format!("cannot delete: {}", e)),
        },
        Act::Fetch(conflict) => {
            let put = || -> io::Result<()> {
                fs::create_dir_all(full.parent().unwrap())?;
                if conflict {
                    fs::rename(&full, conflict_name(&full, &app().cfg().device_name))?;
                }
                match (&tmp, re.hash.strip_prefix("link:")) {
                    (Some(tmp), _) => fs::rename(tmp, &full),
                    (None, Some(target)) => make_link(target, &full),
                    (None, None) => fs::create_dir_all(&full),
                }
            };
            if let Err(e) = put() {
                let _ = tmp.map(fs::remove_file);
                return Err(format!("cannot write (open in another program?): {}", e));
            }
            let mt = if re.is_file() { fs::metadata(&full).map_or(re.mtime, |m| mtime(&m)) } else { 0 };
            ix.take_remote(path, re, mt);
            if re.is_file() {
                f.note(if conflict { "conflict" } else { "got" }, path, &re.by);
            }
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
    let local = f.idx.lock().unwrap().entries.iter().find(|(q, e)| !e.deleted && e.hash == re.hash && *q != path && unchanged_here(&f.root, &folder_path(&f.root, q), Some(e))).map(|(q, _)| folder_path(&f.root, q));
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
        fs::create_dir_all(dir.join("web/x")).unwrap();
        fs::write(dir.join(".gitignore"), "node_modules/\n*.log\n!keep.log\n/build\ndocs/*.tmp\n").unwrap();
        fs::write(dir.join("web/.gitignore"), "*.tmp\n/local\n!important.log\n").unwrap();
        let (mut r, mut items, mut bad) = (Vec::new(), Vec::new(), Vec::new());
        walk(&dir, "", &mut r, &mut items, &mut bad);
        assert!(ignored(&r, "node_modules", true) && !ignored(&r, "node_modules", false));
        assert!(ignored(&r, "x/y.log", false) && !ignored(&r, "keep.log", false));
        assert!(ignored(&r, "build", true) && !ignored(&r, "src/build", true));
        assert!(ignored(&r, "docs/a.tmp", false) && ignored(&r, ".git", true) && ignored(&r, "a/Thumbs.db", false));
        assert!(ignored_path(&r, "node_modules/x/a.js") && !ignored_path(&r, "src/main.rs"));
        // A .gitignore in a folder holds below it only, and over the ones above.
        assert!(ignored(&r, "web/a.tmp", false) && ignored(&r, "web/x/b.tmp", false) && !ignored(&r, "a.tmp", false));
        assert!(ignored(&r, "web/local", true) && !ignored(&r, "web/x/local", true) && !ignored(&r, "local", true));
        assert!(!ignored(&r, "web/important.log", false) && ignored(&r, "important.log", false));
        assert!(items.iter().any(|i| i.path == "web/x" && i.hash.as_deref() == Some("dir")));
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
