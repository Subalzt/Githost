//! A folder's index: every file it has had, with its hash and a version vector (how many times
//! each laptop has changed it). Comparing two vectors tells whether one version came after the
//! other or whether the two were changed apart, which is a conflict. Each entry also carries the
//! local sequence number it last changed at, so a laptop asks another only for what is new.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

use serde_json::{json, Map, Value};

pub type Vv = BTreeMap<String, u64>;

#[derive(Clone, Debug)]
pub struct Entry {
    pub size: u64,
    pub mtime: i64,
    pub hash: String,
    pub deleted: bool,
    pub vv: Vv,
    pub seq: u64,
    pub by: String,
}

#[derive(PartialEq, Debug)]
pub enum Order {
    Equal,
    After,
    Before,
    Apart,
}

/// How `a` stands to `b`.
pub fn order(a: &Vv, b: &Vv) -> Order {
    let (mut more, mut less) = (false, false);
    for (k, &x) in a {
        let y = b.get(k).copied().unwrap_or(0);
        if x > y {
            more = true;
        } else if x < y {
            less = true;
        }
    }
    for (k, &y) in b {
        if !a.contains_key(k) && y > 0 {
            less = true;
        }
    }
    match (more, less) {
        (false, false) => Order::Equal,
        (true, false) => Order::After,
        (false, true) => Order::Before,
        (true, true) => Order::Apart,
    }
}

pub fn merge(a: &Vv, b: &Vv) -> Vv {
    let mut m = a.clone();
    for (k, &v) in b {
        let e = m.entry(k.clone()).or_insert(0);
        *e = (*e).max(v);
    }
    m
}

pub struct Index {
    pub entries: BTreeMap<String, Entry>,
    pub seq: u64,
    /// The last sequence number taken from each other laptop.
    pub since: HashMap<String, u64>,
    /// When this folder's commit last moved here, and to which commit: the newest wins.
    pub git_at: u64,
    pub git_head: String,
    pub dirty: bool,
    file: PathBuf,
}

impl Entry {
    pub fn to_json(&self, path: &str) -> Value {
        json!({"path": path, "size": self.size, "mtime": self.mtime, "hash": self.hash,
               "deleted": self.deleted, "vv": self.vv, "seq": self.seq, "by": self.by})
    }

    pub fn from_json(v: &Value) -> Option<(String, Entry)> {
        let vv = v.get("vv")?.as_object()?.iter().filter_map(|(k, x)| Some((k.clone(), x.as_u64()?))).collect();
        Some((
            v.get("path")?.as_str()?.to_string(),
            Entry {
                size: v.get("size")?.as_u64()?,
                mtime: v.get("mtime")?.as_i64()?,
                hash: v.get("hash")?.as_str()?.to_string(),
                deleted: v.get("deleted")?.as_bool()?,
                vv,
                seq: v.get("seq")?.as_u64()?,
                by: v.get("by")?.as_str()?.to_string(),
            },
        ))
    }
}

/// A path another laptop sends is used only when it stays inside the folder.
pub fn safe_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.contains(':')
        && !p.contains('\0')
        && p.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

impl Index {
    pub fn load(file: PathBuf) -> Index {
        let v: Value = fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
        let mut ix = Index {
            entries: BTreeMap::new(),
            seq: v.get("seq").and_then(|x| x.as_u64()).unwrap_or(0),
            since: v
                .get("since")
                .and_then(|x| x.as_object())
                .map(|o| o.iter().filter_map(|(k, x)| Some((k.clone(), x.as_u64()?))).collect())
                .unwrap_or_default(),
            git_at: v.get("git_at").and_then(|x| x.as_u64()).unwrap_or(0),
            git_head: v.get("git_head").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            dirty: false,
            file,
        };
        if let Some(a) = v.get("entries").and_then(|x| x.as_array()) {
            for e in a {
                if let Some((p, e)) = Entry::from_json(e) {
                    ix.entries.insert(p, e);
                }
            }
        }
        ix
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let v = json!({
            "seq": self.seq,
            "since": self.since,
            "git_at": self.git_at,
            "git_head": self.git_head,
            "entries": self.entries.iter().map(|(p, e)| e.to_json(p)).collect::<Vec<_>>(),
        });
        let tmp = self.file.with_extension("tmp");
        if fs::write(&tmp, serde_json::to_vec(&v).unwrap()).is_ok() && fs::rename(&tmp, &self.file).is_ok() {
            self.dirty = false;
        }
    }

    /// Records a change made on this laptop.
    pub fn local_change(&mut self, me: &str, path: &str, size: u64, mtime: i64, hash: String, deleted: bool) {
        self.seq += 1;
        let seq = self.seq;
        let e = self.entries.entry(path.to_string()).or_insert_with(|| Entry {
            size: 0,
            mtime: 0,
            hash: String::new(),
            deleted,
            vv: Vv::new(),
            seq: 0,
            by: String::new(),
        });
        *e.vv.entry(me.to_string()).or_insert(0) += 1;
        e.size = size;
        e.mtime = mtime;
        e.hash = hash;
        e.deleted = deleted;
        e.seq = seq;
        e.by = me.to_string();
        self.dirty = true;
    }

    /// Takes another laptop's version, keeping what this laptop knew in the vector.
    /// `mtime` is the time the file has here after writing it.
    pub fn take_remote(&mut self, path: &str, r: &Entry, mtime: i64) {
        self.seq += 1;
        let vv = match self.entries.get(path) {
            Some(l) => merge(&l.vv, &r.vv),
            None => r.vv.clone(),
        };
        self.entries.insert(
            path.to_string(),
            Entry { size: r.size, mtime, hash: r.hash.clone(), deleted: r.deleted, vv, seq: self.seq, by: r.by.clone() },
        );
        self.dirty = true;
    }

    /// Notes that this laptop already has another's version (same content), without touching the file.
    pub fn merge_vv(&mut self, path: &str, r: &Vv) {
        if let Some(l) = self.entries.get_mut(path) {
            let m = merge(&l.vv, r);
            if m != l.vv {
                l.vv = m;
                self.seq += 1;
                l.seq = self.seq;
                self.dirty = true;
            }
        }
    }

    pub fn changes_since(&self, since: u64) -> Vec<Value> {
        let mut v: Vec<(&String, &Entry)> = self.entries.iter().filter(|(_, e)| e.seq > since).collect();
        v.sort_by_key(|(_, e)| e.seq);
        v.into_iter().map(|(p, e)| e.to_json(p)).collect()
    }

    pub fn files_and_bytes(&self) -> (u64, u64) {
        self.entries.values().filter(|e| !e.deleted).fold((0, 0), |(n, b), e| (n + 1, b + e.size))
    }

    /// Who last changed each file that is still there, for showing next to uncommitted changes.
    pub fn changed_by(&self) -> Map<String, Value> {
        self.entries.iter().map(|(p, e)| (p.clone(), Value::String(e.by.clone()))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vv(p: &[(&str, u64)]) -> Vv {
        p.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn orders() {
        assert_eq!(order(&vv(&[("a", 1)]), &vv(&[("a", 1)])), Order::Equal);
        assert_eq!(order(&vv(&[("a", 2)]), &vv(&[("a", 1)])), Order::After);
        assert_eq!(order(&vv(&[("a", 1)]), &vv(&[("a", 1), ("b", 1)])), Order::Before);
        assert_eq!(order(&vv(&[("a", 2)]), &vv(&[("a", 1), ("b", 1)])), Order::Apart);
        assert_eq!(order(&vv(&[]), &vv(&[("b", 1)])), Order::Before);
    }

    #[test]
    fn paths() {
        assert!(safe_path("a/b.txt"));
        assert!(!safe_path("../x"));
        assert!(!safe_path("a/../../x"));
        assert!(!safe_path("/etc/passwd"));
        assert!(!safe_path("C:/x"));
        assert!(!safe_path("a\\b"));
        assert!(!safe_path("a//b"));
    }
}
