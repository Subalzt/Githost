//! Notices changes made on this laptop: every second and a half the folder is walked, and any
//! file whose size or time differs from the index is hashed; a new hash is a new version. A file
//! the index has that is no longer there is marked deleted, so the other laptops delete it too.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::folder_path;
use crate::ignore::Rules;
use crate::state::{app, Folder};
use crate::util::{hash_file, mtime_ms};

pub fn run(f: Arc<Folder>) {
    while !f.gone() {
        scan_once(&f);
        thread::sleep(Duration::from_millis(1500));
    }
}

struct Walk {
    files: Vec<(String, u64, i64)>,
    /// Folders that could not be read: what the index has inside them is left alone.
    unreadable: Vec<String>,
}

fn walk(root: &Path, rel: &str, rules: &Rules, out: &mut Walk) {
    let dir = if rel.is_empty() { root.to_path_buf() } else { folder_path(root, rel) };
    let Ok(rd) = fs::read_dir(&dir) else {
        out.unreadable.push(rel.to_string());
        return;
    };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let p = if rel.is_empty() { name } else { format!("{}/{}", rel, name) };
        if ft.is_dir() {
            if !rules.ignored(&p, true) {
                walk(root, &p, rules, out);
            }
        } else if ft.is_file() && !rules.ignored(&p, false) {
            if let Ok(md) = e.metadata() {
                out.files.push((p, md.len(), mtime_ms(&md)));
            }
        }
    }
}

/// Whether a path, or any folder it is in, is kept off the other laptops.
pub fn ignored_path(rules: &Rules, p: &str) -> bool {
    let mut at = 0;
    while let Some(i) = p[at..].find('/') {
        if rules.ignored(&p[..at + i], true) {
            return true;
        }
        at += i + 1;
    }
    rules.ignored(p, false)
}

pub fn scan_once(f: &Folder) {
    if !f.root.is_dir() {
        f.st.lock().unwrap().error = format!("{} is not there", f.root.display());
        return;
    }
    let rules = Rules::load(&f.root);
    let mut w = Walk { files: Vec::new(), unreadable: Vec::new() };
    walk(&f.root, "", &rules, &mut w);
    let me = app().me();

    // Which files look changed (size or time), noted with the index entry as it was.
    let todo: Vec<(String, u64, i64, Option<u64>)> = {
        let ix = f.idx.lock().unwrap();
        w.files
            .iter()
            .filter_map(|(p, size, mt)| match ix.entries.get(p) {
                Some(e) if !e.deleted && e.size == *size && e.mtime == *mt => None,
                other => Some((p.clone(), *size, *mt, other.map(|e| e.seq))),
            })
            .collect()
    };
    let hashed: Vec<(String, u64, i64, Option<u64>, String)> = todo
        .into_iter()
        .filter_map(|(p, size, mt, seq)| {
            let full: PathBuf = folder_path(&f.root, &p);
            hash_file(&full).ok().map(|h| (p, size, mt, seq, h))
        })
        .collect();

    let mut ix = f.idx.lock().unwrap();
    for (p, size, mt, seq, h) in hashed {
        if ix.entries.get(&p).map(|e| e.seq) != seq {
            continue; // taken from another laptop while it was being hashed
        }
        match fs::metadata(folder_path(&f.root, &p)) {
            Ok(md) if md.len() == size && mtime_ms(&md) == mt => {}
            _ => continue, // still being written; next time
        }
        match ix.entries.get_mut(&p) {
            Some(e) if !e.deleted && e.hash == h => {
                e.mtime = mt;
                ix.dirty = true;
            }
            _ => {
                ix.local_change(&me, &p, size, mt, h, false);
                f.note("changed", &p, &me);
            }
        }
    }

    let seen: HashSet<&str> = w.files.iter().map(|(p, _, _)| p.as_str()).collect();
    let missing: Vec<String> = ix
        .entries
        .iter()
        .filter(|(p, e)| !e.deleted && !seen.contains(p.as_str()))
        .filter(|(p, _)| !w.unreadable.iter().any(|u| u.is_empty() || p.starts_with(&format!("{}/", u))))
        .map(|(p, _)| p.clone())
        .collect();
    // A folder that suddenly looks empty (a drive unplugged, a network share gone) is only taken
    // as emptied when it stays so for a few looks.
    let live = ix.entries.values().filter(|e| !e.deleted).count();
    let looks_wiped = w.files.is_empty() && live > 0;
    {
        let mut st = f.st.lock().unwrap();
        st.empty_scans = if looks_wiped { st.empty_scans + 1 } else { 0 };
        if looks_wiped && st.empty_scans < 4 {
            return;
        }
        if st.error.ends_with("is not there") {
            st.error.clear();
        }
    }
    for p in missing {
        if folder_path(&f.root, &p).exists() || ignored_path(&rules, &p) {
            continue;
        }
        ix.local_change(&me, &p, 0, 0, String::new(), true);
        f.note("deleted", &p, &me);
    }
    ix.save();
}
