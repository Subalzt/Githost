//! The running app: its settings, its folders with their indexes and recent activity, and the
//! laptops it has seen.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};
use std::thread;

use serde_json::Value;

use crate::config::{folder_key, Config, FolderCfg};
use crate::index::Index;
use crate::peers::Peer;
use crate::util::now_ms;

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
}

impl Folder {
    pub fn note(&self, kind: &'static str, path: &str, who: &str) {
        let mut a = self.act.lock().unwrap();
        a.push_front(Activity { at: now_ms(), kind, path: path.to_string(), who: who.to_string() });
        a.truncate(200);
    }

    pub fn name(&self) -> String {
        self.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| self.root.display().to_string())
    }

    pub fn gone(&self) -> bool {
        self.gone.load(Ordering::Relaxed)
    }
}

pub struct App {
    pub data: PathBuf,
    pub port: u16,
    pub disc_port: u16,
    pub disc_send: Vec<u16>,
    pub cfg: Mutex<Config>,
    pub folders: RwLock<Vec<Arc<Folder>>>,
    pub peers: Mutex<HashMap<String, Peer>>,
    names: Mutex<HashMap<String, String>>,
}

static APP: OnceLock<App> = OnceLock::new();

pub fn app() -> &'static App {
    APP.get().expect("app not started")
}

pub fn init(data: PathBuf, port: u16, disc_port: u16, disc_send: Vec<u16>) {
    let cfg = Config::load(&data);
    let names: HashMap<String, String> = fs::read(data.join("names.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.as_object().cloned())
        .map(|o| o.into_iter().filter_map(|(k, v)| Some((k, v.as_str()?.to_string()))).collect())
        .unwrap_or_default();
    let _ = APP.set(App {
        data,
        port,
        disc_port,
        disc_send,
        cfg: Mutex::new(cfg),
        folders: RwLock::new(Vec::new()),
        peers: Mutex::new(HashMap::new()),
        names: Mutex::new(names),
    });
}

impl App {
    pub fn me(&self) -> String {
        self.cfg().device_id.clone()
    }

    pub fn cfg(&self) -> MutexGuard<'_, Config> {
        self.cfg.lock().unwrap()
    }

    pub fn folder(&self, id: &str) -> Option<Arc<Folder>> {
        self.folders.read().unwrap().iter().find(|f| f.id == id).cloned()
    }

    pub fn all(&self) -> Vec<Arc<Folder>> {
        self.folders.read().unwrap().clone()
    }

    /// Starts keeping a folder: its index, and the threads that watch it, sync it and follow its repository.
    pub fn open_folder(&self, fc: &FolderCfg) -> Arc<Folder> {
        let id = fc.id();
        let dir = self.data.join("folders");
        let _ = fs::create_dir_all(&dir);
        let f = Arc::new(Folder {
            id: id.clone(),
            key: folder_key(&fc.code),
            code: fc.code.clone(),
            root: PathBuf::from(&fc.path),
            idx: Mutex::new(Index::load(dir.join(format!("{}.json", id)))),
            act: Mutex::new(VecDeque::new()),
            st: Mutex::new(Status::default()),
            git: Mutex::new(crate::git::State::default()),
            git_busy: AtomicBool::new(false),
            gone: AtomicBool::new(false),
        });
        let _ = fs::remove_dir_all(f.root.join(crate::ignore::SCRATCH));
        self.folders.write().unwrap().push(f.clone());
        let a = f.clone();
        thread::spawn(move || crate::scan::run(a));
        let a = f.clone();
        thread::spawn(move || crate::sync::run(a));
        let a = f.clone();
        thread::spawn(move || crate::git::run(a));
        f
    }

    pub fn close_folder(&self, id: &str) {
        let mut v = self.folders.write().unwrap();
        if let Some(f) = v.iter().find(|f| f.id == id) {
            f.gone.store(true, Ordering::Relaxed);
        }
        v.retain(|f| f.id != id);
        let _ = fs::remove_file(self.data.join("folders").join(format!("{}.json", id)));
    }

    /// A laptop's name by its id: this one's, a laptop on the Wi-Fi, or one seen before.
    pub fn name_of(&self, id: &str) -> String {
        if id.is_empty() {
            return String::new();
        }
        {
            let c = self.cfg();
            if id == c.device_id {
                return c.device_name.clone();
            }
        }
        if let Some(n) = self.names.lock().unwrap().get(id) {
            return n.clone();
        }
        format!("Laptop {}", &id[..id.len().min(4)])
    }

    pub fn learn_name(&self, id: &str, name: &str) {
        if name.is_empty() || id.is_empty() {
            return;
        }
        let mut n = self.names.lock().unwrap();
        if n.get(id).map(|s| s.as_str()) != Some(name) {
            n.insert(id.to_string(), name.to_string());
            let _ = fs::write(self.data.join("names.json"), serde_json::to_vec(&*n).unwrap());
        }
    }
}
