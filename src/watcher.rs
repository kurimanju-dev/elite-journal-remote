use anyhow::Result;
use notify::{Config, RecursiveMode, Watcher};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use tokio::sync::watch;

#[cfg(target_os = "macos")]
type NativeWatcher = notify::KqueueWatcher;
#[cfg(not(target_os = "macos"))]
type NativeWatcher = notify::RecommendedWatcher;

pub struct Signals {
    watcher: Option<NativeWatcher>,
    paths: BTreeMap<PathBuf, u64>,
    root: PathBuf,
    pub rx: watch::Receiver<u64>,
    _tx: watch::Sender<u64>,
}

impl Signals {
    pub fn new(root: &Path) -> Self {
        let (tx, rx) = watch::channel(0u64);
        let sender = tx.clone();
        let callback = move |event: notify::Result<notify::Event>| {
            if let Err(error) = event {
                tracing::warn!(%error, "watcher error; reconciliation remains active");
            }
            sender.send_modify(|n| *n = n.wrapping_add(1));
        };
        let watcher =
            match NativeWatcher::new(callback, Config::default().with_follow_symlinks(false)) {
                Ok(watcher) => Some(watcher),
                Err(error) => {
                    tracing::warn!(%error, "watcher unavailable; using periodic reconciliation");
                    None
                }
            };
        let mut signals = Self {
            watcher,
            paths: BTreeMap::new(),
            root: root.to_owned(),
            rx,
            _tx: tx,
        };
        if let Err(error) = signals.refresh() {
            tracing::warn!(%error, "watch registration failed");
        }
        signals
    }

    pub fn refresh(&mut self) -> Result<()> {
        let Some(watcher) = &mut self.watcher else {
            return Ok(());
        };
        let mut desired = BTreeMap::new();
        let meta = fs::metadata(&self.root)?;
        #[cfg(unix)]
        let root_id = {
            use std::os::unix::fs::MetadataExt;
            meta.ino()
        };
        #[cfg(not(unix))]
        let root_id = {
            let _ = meta;
            0
        };
        desired.insert(self.root.clone(), root_id);
        // kqueue watches individual vnodes: directory-only watching misses writes.
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            if crate::files::kind(&name.to_string_lossy()).is_none() {
                continue;
            }
            let meta = match fs::symlink_metadata(entry.path()) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if !crate::files::ordinary(&meta) {
                continue;
            }
            #[cfg(unix)]
            let id = {
                use std::os::unix::fs::MetadataExt;
                meta.ino()
            };
            #[cfg(not(unix))]
            let id = 0;
            desired.insert(entry.path(), id);
        }
        let mut removed = Vec::new();
        for path in self.paths.keys() {
            if !desired.contains_key(path) {
                removed.push(path.clone());
            }
        }
        for path in removed {
            let _ = watcher.unwatch(&path);
            self.paths.remove(&path);
        }
        for (path, id) in desired {
            if self.paths.get(&path) == Some(&id) {
                continue;
            }
            if self.paths.remove(&path).is_some() {
                let _ = watcher.unwatch(&path);
            }
            match watcher.watch(&path, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.paths.insert(path, id);
                }
                Err(error) => {
                    tracing::warn!(file = %path.display(), %error, "watch registration deferred")
                }
            }
        }
        Ok(())
    }
}
