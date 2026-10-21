use crate::protocol::{FileState, Inventory, Kind, MAX_PAYLOAD, digest};
use anyhow::{Context, Result, ensure};
use clap::ValueEnum;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub fn kind(name: &str) -> Option<Kind> {
    if name.len() > 180 || name.starts_with('.') || name.ends_with('.') {
        return None;
    }
    for byte in name.bytes() {
        if !byte.is_ascii_alphanumeric() && !b"._-".contains(&byte) {
            return None;
        }
    }
    let lower = name.to_ascii_lowercase();
    let stem = lower.split('.').next()?;
    for reserved in [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ] {
        if stem == reserved {
            return None;
        }
    }
    if lower.starts_with("journal.") && lower.ends_with(".log") {
        return Some(Kind::Journal);
    }
    if lower.ends_with(".json") {
        return Some(Kind::Snapshot);
    }
    None
}

pub fn ordinary(meta: &fs::Metadata) -> bool {
    if !meta.is_file() || meta.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return false;
        } // reparse point
    }
    true
}

pub fn checked_path(root: &Path, name: &str) -> Result<PathBuf> {
    ensure!(
        kind(name).is_some(),
        "unsafe or unsupported filename: {name}"
    );
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            ensure!(
                ordinary(&meta),
                "refusing non-regular file: {}",
                path.display()
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(meta.nlink() == 1, "refusing hard-linked destination");
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(path)
}

pub fn hash_prefix(file: &mut File, len: u64) -> Result<[u8; 32]> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = blake3::Hasher::new();
    let mut reader = file.take(len);
    let copied = std::io::copy(&mut reader, &mut hasher)?;
    ensure!(copied == len, "file changed while hashing");
    Ok(*hasher.finalize().as_bytes())
}

pub fn state(path: &Path) -> Result<FileState> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let hash = hash_prefix(&mut f, len)?;
    Ok(FileState { len, hash })
}

pub fn inventory(root: &Path) -> Result<Inventory> {
    let mut result = Inventory::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let filename = entry.file_name();
        let name = match filename.to_str() {
            Some(text) => text.to_string(),
            None => continue,
        };
        if kind(&name).is_none() {
            continue;
        }
        let path = checked_path(root, &name)?;
        ensure!(
            result.len() < 4096,
            "too many mirrored files (maximum 4096)"
        );
        result.insert(name, state(&path)?);
    }
    Ok(result)
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum SnapshotMode {
    Overwrite,
    Atomic,
}

pub struct Destination {
    pub root: PathBuf,
    mode: SnapshotMode,
    _lock: File,
}

impl Destination {
    pub fn open(root: &Path, mode: SnapshotMode) -> Result<Self> {
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        let lock_path = root.join(".elite-journal-relay.lock");
        if let Ok(meta) = fs::symlink_metadata(&lock_path) {
            ensure!(ordinary(&meta), "unsafe receiver lock file");
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        lock.try_lock()
            .context("another receiver is using this destination")?;
        Ok(Self {
            root,
            mode,
            _lock: lock,
        })
    }

    pub fn apply(&self, update: &crate::source::Update) -> Result<()> {
        let name = &update.name;
        let data = &update.data;
        let result = &update.result;
        let offset = update.offset;
        let reset = update.reset;
        ensure!(kind(name) == Some(update.kind), "file kind mismatch");
        ensure!(data.len() <= MAX_PAYLOAD, "payload too large");
        let path = checked_path(&self.root, name)?;
        if update.kind == Kind::Snapshot {
            ensure!(
                offset == 0
                    && reset
                    && result.len == data.len() as u64
                    && result.hash == digest(data),
                "invalid snapshot digest/length"
            );
            ensure!(
                serde_json::from_slice::<serde_json::Value>(data)?.is_object(),
                "snapshot is not a JSON object"
            );
            if path.exists() && state(&path)? == *result {
                return Ok(());
            }
            match self.mode {
                SnapshotMode::Overwrite => {
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(&path)?;
                    file.write_all(data)?;
                    file.set_len(data.len() as u64)?;
                    file.sync_all()?;
                    drop(file);
                }
                SnapshotMode::Atomic => {
                    let mut stage = tempfile::Builder::new()
                        .prefix(".relay-stage-")
                        .tempfile_in(&self.root)?;
                    stage.write_all(data)?;
                    stage.as_file().sync_all()?;
                    let file = stage.persist(&path).context("atomic snapshot replacement (try --snapshot-mode overwrite for locked files)")?;
                    file.sync_all()?;
                }
            }
        } else {
            ensure!(
                data.is_empty() || data.ends_with(b"\n"),
                "journal patch ends with a partial record"
            );
            ensure!(!reset || offset == 0, "reset offset must be zero");
            ensure!(
                result.len
                    == offset
                        .checked_add(data.len() as u64)
                        .context("offset overflow")?,
                "invalid journal length"
            );
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            let actual_len = file.metadata()?.len();
            // A lost ACK can send the same patch twice. Don't write it again.
            if actual_len == result.len && hash_prefix(&mut file, actual_len)? == result.hash {
                return Ok(());
            }
            if reset {
                if actual_len != 0 {
                    let mut backup = tempfile::Builder::new()
                        .prefix(".relay-conflict-")
                        .suffix(".bak")
                        .tempfile_in(&self.root)?;
                    let mut original = File::open(&path)?;
                    std::io::copy(&mut original, &mut backup)?;
                    backup.as_file().sync_all()?;
                    let (_, saved) = backup.keep()?;
                    tracing::warn!(original = %path.display(), backup = %saved.display(), "saved conflicting journal before repair");
                }
            } else {
                ensure!(
                    actual_len == offset,
                    "journal offset mismatch; reconnect to reconcile"
                );
                ensure!(
                    hash_prefix(&mut file, offset)? == update.prefix_hash,
                    "journal prefix mismatch; reconnect to reconcile"
                );
            }
            let mut hasher = blake3::Hasher::new();
            if !reset {
                file.seek(SeekFrom::Start(0))?;
                std::io::copy(&mut (&mut file).take(offset), &mut hasher)?;
            }
            hasher.update(data);
            ensure!(
                hasher.finalize().as_bytes() == &result.hash,
                "invalid journal result hash"
            );
            if reset {
                file.set_len(0)?;
            }
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(data)?;
            file.set_len(result.len)?;
            set_creation_time(&file, update.created_unix_ms)?;
            file.sync_all()?; // FlushFileBuffers on Windows, then close before ACK.
            drop(file);
        }
        Ok(())
    }
}

#[cfg(windows)]
fn set_creation_time(file: &File, unix_ms: u64) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{Foundation::FILETIME, Storage::FileSystem::SetFileTime};
    let ticks = unix_ms
        .checked_add(11_644_473_600_000)
        .and_then(|v| v.checked_mul(10_000))
        .context("invalid creation timestamp")?;
    let time = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    // Preserve chronological discovery when historical journals are copied after a live file.
    let ok = unsafe {
        SetFileTime(
            file.as_raw_handle(),
            &time,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    ensure!(ok != 0, "SetFileTime: {}", std::io::Error::last_os_error());
    Ok(())
}

#[cfg(not(windows))]
fn set_creation_time(_: &File, _: u64) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
pub fn default_destination() -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::{
        System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
        UI::Shell::{FOLDERID_SavedGames, KF_FLAG_CREATE, SHGetKnownFolderPath},
    };
    // This function runs synchronously; balance COM initialization on this thread.
    unsafe {
        let initialized = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) >= 0;
        let mut ptr = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(
            &FOLDERID_SavedGames,
            KF_FLAG_CREATE as u32,
            std::ptr::null_mut(),
            &mut ptr,
        );
        let result = if hr >= 0 && !ptr.is_null() {
            let mut len = 0;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            Ok(
                PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(
                    ptr, len,
                )))
                .join("Frontier Developments")
                .join("Elite Dangerous"),
            )
        } else {
            Err(anyhow::anyhow!(
                "SHGetKnownFolderPath failed: {hr:#x}; pass --destination"
            ))
        };
        if !ptr.is_null() {
            CoTaskMemFree(ptr.cast());
        }
        if initialized {
            CoUninitialize();
        }
        result
    }
}

#[cfg(not(windows))]
pub fn default_destination() -> Result<PathBuf> {
    anyhow::bail!("pass --destination on non-Windows systems")
}

pub fn default_source() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let path = default_destination()?;
        ensure!(path.is_dir(), "journal directory not found; pass --source");
        Ok(path)
    }
    #[cfg(not(windows))]
    {
        let mut candidates = Vec::new();
        for profile in crate::locations::discover() {
            if !profile.has_journals {
                continue;
            }
            match profile.destination.canonicalize() {
                Ok(path) => candidates.push(path),
                Err(_) => continue,
            }
        }
        candidates.sort();
        candidates.dedup();
        ensure!(
            candidates.len() == 1,
            "found {} journal directories; select one or pass --source",
            candidates.len()
        );
        Ok(candidates.remove(0))
    }
}
