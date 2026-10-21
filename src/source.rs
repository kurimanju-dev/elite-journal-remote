use crate::{
    files::{hash_prefix, kind, ordinary},
    protocol::{Digest, FileState, Inventory, Kind, MAX_PAYLOAD, digest},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const CHUNK: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct Update {
    pub name: String,
    pub kind: Kind,
    pub offset: u64,
    pub reset: bool,
    pub prefix_hash: Digest,
    pub result: FileState,
    pub created_unix_ms: u64,
    pub data: Vec<u8>,
}

fn stamp(meta: &fs::Metadata) -> (u64, SystemTime, u64) {
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        meta.ino()
    };
    #[cfg(not(unix))]
    let identity = 0;
    let modified = match meta.modified() {
        Ok(time) => time,
        Err(_) => UNIX_EPOCH,
    };
    (meta.len(), modified, identity)
}

pub struct Source {
    root: PathBuf,
    cursors: Inventory,
    seen: BTreeMap<String, (u64, SystemTime, u64)>,
    pub pending: bool,
    chunk_size: usize,
}

impl Source {
    pub fn new(root: PathBuf, cursors: Inventory) -> Result<Self> {
        ensure!(root.is_dir(), "source directory does not exist");
        ensure!(cursors.len() <= 4096, "inventory too large");
        for name in cursors.keys() {
            if kind(name).is_none() {
                anyhow::bail!("invalid inventory filename");
            }
        }
        Ok(Self {
            root,
            cursors,
            seen: BTreeMap::new(),
            pending: false,
            chunk_size: CHUNK,
        })
    }

    pub fn with_chunk_size(mut self, bytes: usize) -> Result<Self> {
        ensure!(
            (16 * 1024..=4 * 1024 * 1024).contains(&bytes),
            "journal chunks must be 16–4096 KiB"
        );
        self.chunk_size = bytes;
        Ok(self)
    }

    pub fn acknowledge(&mut self, update: &Update) {
        self.cursors
            .insert(update.name.clone(), update.result.clone());
    }

    pub fn reconcile_inventory(&mut self, inventory: Inventory) -> Result<()> {
        if inventory.len() > 4096 {
            anyhow::bail!("invalid inventory");
        }
        for name in inventory.keys() {
            ensure!(kind(name).is_some(), "invalid inventory");
        }
        self.cursors = inventory;
        self.seen.clear();
        Ok(())
    }

    /// Send one journal chunk per round so history does not hold up live files.
    pub fn next_round(&mut self, force_verify: bool) -> Result<Vec<Update>> {
        let mut entries = Vec::new();
        let mut folded = HashSet::new();
        for entry in fs::read_dir(&self.root).context("read source directory")? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(file_kind) = kind(&name) else {
                continue;
            };
            let meta = match fs::symlink_metadata(entry.path()) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if !ordinary(&meta) {
                continue;
            }
            ensure!(
                folded.insert(name.to_ascii_lowercase()),
                "case-colliding source filenames are incompatible with Windows"
            );
            ensure!(entries.len() < 4096, "too many source files (maximum 4096)");
            let mut order = 2;
            if file_kind == Kind::Snapshot {
                order = 1;
                if name == "Status.json" {
                    order = 0;
                }
            }
            entries.push((name, file_kind, meta, order));
        }
        entries.sort_by(|a, b| {
            let order = a.3.cmp(&b.3);
            if order != std::cmp::Ordering::Equal {
                return order;
            }
            let left_time = a.2.modified().unwrap_or(UNIX_EPOCH);
            let right_time = b.2.modified().unwrap_or(UNIX_EPOCH);
            let date_order = right_time.cmp(&left_time);
            if date_order != std::cmp::Ordering::Equal {
                return date_order;
            }
            a.0.cmp(&b.0)
        });
        let mut updates = Vec::new();
        let mut batch_bytes = 0usize;
        let mut journal_sent = false;
        self.pending = false;
        for (name, file_kind, meta, _) in entries {
            let file_stamp = stamp(&meta);
            if !force_verify && self.seen.get(&name) == Some(&file_stamp) {
                continue;
            }
            if batch_bytes >= MAX_PAYLOAD {
                self.pending = true;
                continue;
            }
            if file_kind == Kind::Journal && journal_sent {
                self.pending = true;
                continue;
            }
            let path = self.root.join(&name);
            let update = match self.read_update(&path, &name, file_kind, &meta) {
                Ok(u) => u,
                Err(e) => {
                    // The game may have replaced it. Try again next round.
                    tracing::warn!(file = %name, error = %e, "source read deferred");
                    self.pending = true;
                    continue;
                }
            };
            let (update, complete) = update;
            if let Some(update) = update {
                batch_bytes += update.data.len();
                if file_kind == Kind::Journal {
                    journal_sent = true;
                }
                updates.push(update);
            }
            if complete {
                self.seen.insert(name, file_stamp);
            } else {
                self.pending = true;
            }
        }
        Ok(updates)
    }

    fn read_update(
        &self,
        path: &Path,
        name: &str,
        file_kind: Kind,
        meta: &fs::Metadata,
    ) -> Result<(Option<Update>, bool)> {
        let mut file = File::open(path)?;
        ensure!(
            stamp(&file.metadata()?) == stamp(meta),
            "file replaced before read"
        );
        let created = match meta.created() {
            Ok(time) => time,
            Err(_) => meta.modified()?,
        };
        let elapsed = created.duration_since(UNIX_EPOCH).unwrap_or_default();
        let created_unix_ms = match u64::try_from(elapsed.as_millis()) {
            Ok(ms) => ms,
            Err(_) => u64::MAX,
        };
        if file_kind == Kind::Snapshot {
            ensure!(
                meta.len() <= MAX_PAYLOAD as u64,
                "snapshot exceeds 16 MiB limit"
            );
            let mut data = Vec::new();
            (&mut file)
                .take(MAX_PAYLOAD as u64 + 1)
                .read_to_end(&mut data)?;
            ensure!(data.len() <= MAX_PAYLOAD, "snapshot exceeds 16 MiB limit");
            ensure!(
                stamp(&file.metadata()?) == stamp(meta),
                "snapshot changed while reading"
            );
            let parsed = serde_json::from_slice::<serde_json::Value>(&data);
            match parsed {
                Ok(value) if value.is_object() => (),
                _ => return Ok((None, false)),
            }
            let result = FileState {
                len: data.len() as u64,
                hash: digest(&data),
            };
            if self.cursors.get(name) == Some(&result) {
                return Ok((None, true));
            }
            return Ok((
                Some(Update {
                    name: name.into(),
                    kind: file_kind,
                    offset: 0,
                    reset: true,
                    prefix_hash: digest(&[]),
                    result,
                    created_unix_ms,
                    data,
                }),
                true,
            ));
        }
        let cursor = self.cursors.get(name);
        let mut offset = match cursor {
            Some(saved) => saved.len,
            None => 0,
        };
        let mut prefix_hash = digest(&[]);
        let mut reset = cursor.is_none();
        if let Some(cursor) = cursor {
            if cursor.len <= meta.len() && hash_prefix(&mut file, cursor.len)? == cursor.hash {
                prefix_hash = cursor.hash;
            } else {
                offset = 0;
                reset = true;
            }
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut data = Vec::with_capacity(self.chunk_size);
        // Grow only for a single unusually large journal record; never split a line.
        loop {
            let before = data.len();
            (&mut file)
                .take(self.chunk_size.min(MAX_PAYLOAD - before) as u64)
                .read_to_end(&mut data)?;
            if let Some(last) = data.iter().rposition(|b| *b == b'\n') {
                data.truncate(last + 1);
                break;
            }
            if data.len() == before {
                if data.is_empty() && offset == meta.len() && !reset {
                    return Ok((None, true));
                }
                if data.is_empty() && meta.len() == 0 && reset {
                    let update = Update {
                        name: name.to_string(),
                        kind: file_kind,
                        offset: 0,
                        reset: true,
                        prefix_hash,
                        result: FileState {
                            len: 0,
                            hash: digest(&[]),
                        },
                        created_unix_ms,
                        data,
                    };
                    return Ok((Some(update), true));
                }
                return Ok((None, false));
            }
            ensure!(
                data.len() < MAX_PAYLOAD,
                "journal record exceeds 16 MiB limit"
            );
        }
        // Size alone won't catch a file being replaced during the read.
        let mut hasher = blake3::Hasher::new();
        file.seek(SeekFrom::Start(0))?;
        let copied = std::io::copy(&mut (&mut file).take(offset), &mut hasher)?;
        ensure!(
            copied == offset && hasher.finalize().as_bytes() == &prefix_hash,
            "journal prefix changed while reading"
        );
        hasher.update(&data);
        let result = FileState {
            len: offset + data.len() as u64,
            hash: *hasher.finalize().as_bytes(),
        };
        ensure!(
            stamp(&file.metadata()?) == stamp(meta),
            "journal changed while reading"
        );
        let complete = result.len == meta.len();
        let update = Update {
            name: name.into(),
            kind: file_kind,
            offset,
            reset,
            prefix_hash,
            result,
            created_unix_ms,
            data,
        };
        Ok((Some(update), complete))
    }
}
