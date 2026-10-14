//! What a laptop keeps: its settings, and per folder an index of every file it has had, with its
//! hash and a version vector (how many times each laptop changed it). Comparing two vectors tells
//! whether one version came after the other or the two were changed apart, which is a conflict.
//! Each entry also carries the local sequence number it changed at, so laptops ask only for what is new.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{hex, net::Peer, now_ms, random, sha};

pub fn read_json<T: DeserializeOwned + Default>(p: &Path) -> T {
    fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn write_json(p: &Path, v: &impl Serialize) -> bool {
    let tmp = p.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(v).unwrap()).is_ok() && fs::rename(&tmp, p).is_ok()
}

/// A folder is known by its code: its id and the key its requests are signed with both come from it.
/// Letters and digits that cannot be mistaken for one another (no 0/O, 1/I).
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

pub fn new_code() -> String {
    random(12).iter().map(|b| ALPHABET[(*b % 32) as usize] as char).collect()
}

pub fn normalize_code(s: &str) -> Option<String> {
    let c: String = s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect();
    (c.len() == 12 && c.bytes().all(|b| ALPHABET.contains(&b))).then_some(c)
}

pub fn pretty_code(c: &str) -> String {
    c.as_bytes().chunks(4).map(|ch| String::from_utf8_lossy(ch)).collect::<Vec<_>>().join("-")
}

pub fn folder_id(code: &str) -> String {
    hex(&sha(&format!("foldersync id {}", code)))[..24].to_string()
}

#[derive(Serialize, Deserialize, Clone)]
pub struct FolderCfg {
    pub code: String,
    pub path: String,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub device_id: String,
    pub device_name: String,
    pub folders: Vec<FolderCfg>,
    pub manual_peers: Vec<String>,
    /// Names of laptops seen before, for showing who changed what.
    pub names: HashMap<String, String>,
}

pub type Vv = BTreeMap<String, u64>;

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
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
    let more = a.iter().any(|(k, &x)| x > b.get(k).copied().unwrap_or(0));
    let less = b.iter().any(|(k, &y)| y > a.get(k).copied().unwrap_or(0));
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

/// A path another laptop sends is used only when it stays inside the folder.
pub fn safe_path(p: &str) -> bool {
    !p.is_empty() && !p.starts_with('/') && !p.contains(['\\', ':', '\0']) && p.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Index {
    pub entries: BTreeMap<String, Entry>,
    pub seq: u64,
    /// The last sequence number taken from each other laptop.
    pub since: HashMap<String, u64>,
    /// When this folder's commit last moved here, and to which: the newest move wins.
    pub git_at: u64,
    pub git_head: String,
    #[serde(skip)]
    pub dirty: bool,
}

impl Index {
    /// Records a change made on this laptop.
    pub fn local_change(&mut self, me: &str, path: &str, size: u64, mtime: i64, hash: String, deleted: bool) {
        self.seq += 1;
        let e = self.entries.entry(path.to_string()).or_default();
        *e.vv.entry(me.to_string()).or_insert(0) += 1;
        (e.size, e.mtime, e.hash, e.deleted, e.seq, e.by) = (size, mtime, hash, deleted, self.seq, me.to_string());
        self.dirty = true;
    }

    /// Takes another laptop's version, keeping what this laptop knew in the vector; `mtime` is the file's time here.
    pub fn take_remote(&mut self, path: &str, r: &Entry, mtime: i64) {
        self.seq += 1;
        let vv = self.entries.get(path).map_or(r.vv.clone(), |l| merge(&l.vv, &r.vv));
        self.entries.insert(path.to_string(), Entry { mtime, vv, seq: self.seq, ..r.clone() });
        self.dirty = true;
    }

    /// Notes that this laptop already has another's version (the same content) without touching the file.
    pub fn merge_vv(&mut self, path: &str, r: &Vv) {
        if let Some(l) = self.entries.get_mut(path) {
            let m = merge(&l.vv, r);
            if m != l.vv {
                self.seq += 1;
                (l.vv, l.seq, self.dirty) = (m, self.seq, true);
            }
        }
    }

    pub fn changes_since(&self, since: u64) -> Vec<(&String, &Entry)> {
        let mut v: Vec<_> = self.entries.iter().filter(|(_, e)| e.seq > since).collect();
        v.sort_by_key(|(_, e)| e.seq);
        v
    }

    pub fn files_and_bytes(&self) -> (u64, u64) {
        self.entries.values().filter(|e| !e.deleted).fold((0, 0), |(n, b), e| (n + 1, b + e.size))
    }
}

pub struct Activity {
    pub at: u64,
    pub kind: &'static str,
    pub path: String,
    pub who: String,
}

#[derive(Default)]
pub struct Status {
    /// Files another laptop has that this one is still to take.
    pub pending: u64,
    pub current: String,
    pub error: String,
    /// When each laptop was last caught up with.
    pub synced_with: HashMap<String, u64>,
    pub empty_scans: u32,
}

pub struct Folder {
    pub id: String,
    pub key: Vec<u8>,
    pub code: String,
    pub root: PathBuf,
    pub idx: Mutex<Index>,
    pub act: Mutex<VecDeque<Activity>>,
    pub st: Mutex<Status>,
    pub git: Mutex<crate::git::State>,
    pub git_busy: AtomicBool,
    pub gone: AtomicBool,
    ix_file: PathBuf,
}

impl Folder {
    pub fn note(&self, kind: &'static str, path: &str, who: &str) {
        let mut a = self.act.lock().unwrap();
        a.push_front(Activity { at: now_ms(), kind, path: path.to_string(), who: who.to_string() });
        a.truncate(200);
    }

    pub fn name(&self) -> String {
        self.root.file_name().map_or_else(|| self.root.display().to_string(), |n| n.to_string_lossy().into_owned())
    }

    pub fn gone(&self) -> bool {
        self.gone.load(Relaxed)
    }

    pub fn save(&self, ix: &mut Index) {
        if ix.dirty && write_json(&self.ix_file, ix) {
            ix.dirty = false;
        }
    }
}

pub struct App {
    pub data: PathBuf,
    pub port: u16,
    pub disc_port: u16,
    pub disc_send: Vec<u16>,
    cfg: Mutex<Config>,
    folders: RwLock<Vec<Arc<Folder>>>,
    pub peers: Mutex<HashMap<String, Peer>>,
}

static APP: OnceLock<App> = OnceLock::new();

pub fn app() -> &'static App {
    APP.get().expect("app not started")
}

pub fn init(data: PathBuf, port: u16, disc_port: u16, disc_send: Vec<u16>) {
    let mut cfg: Config = read_json(&data.join("config.json"));
    if cfg.device_id.is_empty() {
        cfg.device_id = hex(&random(8));
    }
    if cfg.device_name.is_empty() {
        cfg.device_name = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "Laptop".into());
    }
    write_json(&data.join("config.json"), &cfg);
    let _ = APP.set(App { data, port, disc_port, disc_send, cfg: Mutex::new(cfg), folders: Default::default(), peers: Default::default() });
}

impl App {
    pub fn cfg(&self) -> MutexGuard<'_, Config> {
        self.cfg.lock().unwrap()
    }

    pub fn save(&self, c: &Config) {
        write_json(&self.data.join("config.json"), c);
    }

    pub fn me(&self) -> String {
        self.cfg().device_id.clone()
    }

    pub fn folder(&self, id: &str) -> Option<Arc<Folder>> {
        self.folders.read().unwrap().iter().find(|f| f.id == id).cloned()
    }

    pub fn all(&self) -> Vec<Arc<Folder>> {
        self.folders.read().unwrap().clone()
    }

    /// Starts keeping a folder: its index, the thread that syncs it and the one that follows its repository.
    pub fn open_folder(&self, fc: &FolderCfg) {
        let (id, dir) = (folder_id(&fc.code), self.data.join("folders"));
        let _ = fs::create_dir_all(&dir);
        let ix_file = dir.join(format!("{}.json", id));
        let f = Arc::new(Folder {
            key: sha(&format!("foldersync key {}", fc.code)),
            code: fc.code.clone(),
            root: PathBuf::from(&fc.path),
            idx: Mutex::new(read_json(&ix_file)),
            act: Default::default(),
            st: Default::default(),
            git: Default::default(),
            git_busy: Default::default(),
            gone: Default::default(),
            ix_file,
            id,
        });
        let _ = fs::remove_dir_all(f.root.join(crate::sync::SCRATCH));
        self.folders.write().unwrap().push(f.clone());
        let g = f.clone();
        std::thread::spawn(move || crate::sync::run(f));
        std::thread::spawn(move || crate::git::run(g));
    }

    pub fn close_folder(&self, id: &str) {
        let mut v = self.folders.write().unwrap();
        v.iter().filter(|f| f.id == id).for_each(|f| f.gone.store(true, Relaxed));
        v.retain(|f| f.id != id);
        let _ = fs::remove_file(self.data.join("folders").join(format!("{}.json", id)));
    }

    /// A laptop's name by its id: this one's, or one seen before.
    pub fn name_of(&self, id: &str) -> String {
        let c = self.cfg();
        if id == c.device_id {
            return c.device_name.clone();
        }
        c.names.get(id).cloned().unwrap_or_else(|| format!("Laptop {}", &id[..id.len().min(4)]))
    }

    pub fn learn_name(&self, id: &str, name: &str) {
        let mut c = self.cfg();
        if !name.is_empty() && c.names.get(id).map(String::as_str) != Some(name) {
            c.names.insert(id.to_string(), name.to_string());
            self.save(&c);
        }
    }
}
