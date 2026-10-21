use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};

const JOURNALS: &str = "Saved Games/Frontier Developments/Elite Dangerous";

#[derive(Clone, Debug, Serialize)]
pub struct Profile {
    pub prefix: PathBuf,
    pub profile: String,
    pub destination: PathBuf,
    pub has_journals: bool,
}

pub fn profiles(prefix: &Path) -> Result<Vec<Profile>> {
    let prefix = if prefix.join("pfx/drive_c").is_dir() {
        prefix.join("pfx")
    } else {
        prefix.to_owned()
    };
    let prefix = prefix
        .canonicalize()
        .context("Wine prefix does not exist")?;
    ensure!(
        prefix.join("drive_c").is_dir(),
        "select a Wine prefix containing drive_c (or a Proton folder containing pfx)"
    );
    let mut found = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in fs::read_dir(prefix.join("drive_c/users"))
        .context("Wine prefix has no user profiles; initialize it with Wine first")?
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if ["public", "default", "default user", "all users"]
            .contains(&name.to_ascii_lowercase().as_str())
            || !entry.path().is_dir()
        {
            continue;
        }
        let identity = entry.path().canonicalize()?;
        if !seen.insert(identity) {
            continue;
        }
        let destination = entry.path().join(JOURNALS);
        let mut has_journals = false;
        if let Ok(files) = fs::read_dir(&destination) {
            for file in files {
                let file = match file {
                    Ok(file) => file,
                    Err(_) => continue,
                };
                let filename = file.file_name();
                if crate::files::kind(&filename.to_string_lossy()).is_some() {
                    has_journals = true;
                    break;
                }
            }
        }
        found.push(Profile {
            prefix: prefix.clone(),
            profile: name,
            destination,
            has_journals,
        });
    }
    found.sort_by(|a, b| a.profile.cmp(&b.profile));
    Ok(found)
}

pub fn destination(prefix: &Path, user: Option<&str>) -> Result<PathBuf> {
    let all = profiles(prefix)?;
    let mut entries = Vec::new();
    for entry in all {
        match user {
            Some(name) if entry.profile != name => continue,
            _ => entries.push(entry),
        }
    }
    ensure!(
        entries.len() == 1,
        "found {} matching Wine profiles; select a profile or pass --wine-user",
        entries.len()
    );
    Ok(entries.remove(0).destination)
}

/// Only check known locations; don't crawl home or start Wine.
pub fn discover() -> Vec<Profile> {
    let home = match std::env::var_os("HOME") {
        Some(path) => PathBuf::from(path),
        None => match std::env::var_os("USERPROFILE") {
            Some(path) => PathBuf::from(path),
            None => return Vec::new(),
        },
    };
    let data = match std::env::var_os("XDG_DATA_HOME") {
        Some(path) => PathBuf::from(path),
        None => home.join(".local/share"),
    };
    let config = if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(path)
    } else {
        home.join(".config")
    };
    let mut roots = vec![
        home.join(".wine"),
        home.join(".wineprefixes"),
        home.join("Games"),
        data.join("wineprefixes"),
        data.join("bottles/bottles"),
        home.join(".var/app/com.usebottles.bottles/data/bottles/bottles"),
        home.join("Library/Application Support/Highball/bottles"),
        home.join("Library/Application Support/CrossOver/Bottles"),
        PathBuf::from("/Library/Application Support/CrossOver/Bottles"),
    ];
    if let Some(prefix) = std::env::var_os("WINEPREFIX") {
        roots.push(prefix.into());
    }
    if let Some(paths) = std::env::var_os("CX_BOTTLE_PATH") {
        for path in std::env::split_paths(&paths) {
            roots.push(path);
        }
    }
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("/usr/bin/defaults")
        .args(["read", "com.codeweavers.CrossOver", "BottleDir"])
        .output()
        && out.status.success()
    {
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        roots.push(if let Some(rest) = path.strip_prefix("~/") {
            home.join(rest)
        } else {
            PathBuf::from(path)
        });
    }
    let mut steam = vec![
        home.join(".steam/steam"),
        data.join("Steam"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
    ];
    for base in steam.clone() {
        if let Ok(vdf) = fs::read_to_string(base.join("steamapps/libraryfolders.vdf")) {
            for line in vdf.lines() {
                let parts: Vec<_> = line.split('"').collect();
                if parts.len() >= 4 && parts[1] == "path" {
                    steam.push(PathBuf::from(parts[3].replace("\\\\", "\\")));
                }
            }
        }
    }
    for path in steam {
        roots.push(path.join("steamapps/compatdata"));
    }
    // Lutris stores explicit custom prefixes in per-game YAML files.
    for directory in [
        config.join("lutris/games"),
        home.join(".var/app/net.lutris.Lutris/config/lutris/games"),
    ] {
        let files = match fs::read_dir(&directory) {
            Ok(files) => files,
            Err(_) => continue,
        };
        for file in files.flatten() {
            if let Ok(text) = fs::read_to_string(file.path()) {
                for line in text.lines() {
                    if let Some(value) = line.trim().strip_prefix("prefix:") {
                        let value = value.trim().trim_matches(['\'', '"']);
                        let path = match value.strip_prefix("~/") {
                            Some(rest) => home.join(rest),
                            None => PathBuf::from(value),
                        };
                        if path.is_absolute() {
                            roots.push(path);
                        }
                    }
                }
            }
        }
    }
    discover_in(&roots)
}

fn discover_in(roots: &[PathBuf]) -> Vec<Profile> {
    let mut queue = VecDeque::new();
    for root in roots {
        queue.push_back((root.clone(), 0));
    }
    let mut seen = BTreeSet::new();
    let mut found = Vec::new();
    while let Some((path, depth)) = queue.pop_front() {
        if seen.len() >= 4096 {
            break;
        }
        let Ok(path) = path.canonicalize() else {
            continue;
        };
        if !path.is_dir() || !seen.insert(path.clone()) {
            continue;
        }
        if path.join("drive_c").is_dir() {
            if let Ok(mut entries) = profiles(&path) {
                found.append(&mut entries);
            }
            continue;
        }
        if depth < 4 {
            let children = match fs::read_dir(&path) {
                Ok(children) => children,
                Err(_) => continue,
            };
            for child in children.flatten() {
                let child = child.path();
                if child.is_dir() {
                    queue.push_back((child, depth + 1));
                }
            }
        }
    }
    found.sort_by(|a, b| {
        if a.prefix == b.prefix {
            a.profile.cmp(&b.profile)
        } else {
            a.prefix.cmp(&b.prefix)
        }
    });
    found
}
