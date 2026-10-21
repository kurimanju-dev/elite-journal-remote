#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{Context, Result};
use elite_journal_relay::{
    app::{self, ReceiveOptions, ServeOptions},
    discovery,
    files::{self, SnapshotMode},
    locations, protocol,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tracing_subscriber::{
    Layer,
    layer::{Context as LayerContext, SubscriberExt},
    util::SubscriberInitExt,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct Options {
    source: String,
    destination: String,
    connect: String,
    listen: String,
    key_file: String,
    log_file: String,
    verbose: bool,
    discovery: bool,
    once: bool,
    snapshot_mode: String,
    tls: bool,
    tls_cert: String,
    tls_key: String,
    tls_ca: String,
    tls_server_name: String,
    journal_chunk_kib: u64,
    reconcile_ms: u64,
    verify_secs: u64,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            source: String::new(),
            destination: String::new(),
            connect: "auto".into(),
            listen: "0.0.0.0:48933".into(),
            key_file: String::new(),
            log_file: String::new(),
            verbose: false,
            discovery: true,
            once: false,
            snapshot_mode: "overwrite".into(),
            tls: false,
            tls_cert: String::new(),
            tls_key: String::new(),
            tls_ca: String::new(),
            tls_server_name: "elite-relay".into(),
            journal_chunk_kib: 1024,
            reconcile_ms: 250,
            verify_secs: 30,
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    role: Role,
    send: Options,
    receive: Options,
    remotes: Vec<String>,
}
#[derive(Clone, Copy, Default, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    Send,
    #[default]
    Receive,
}

#[derive(Clone, Serialize)]
struct Log {
    id: u64,
    time: u64,
    level: String,
    message: String,
    fields: BTreeMap<String, String>,
}
#[derive(Default)]
struct Activity {
    running: bool,
    phase: String,
    error: Option<String>,
    updates: u64,
    bytes: u64,
    sequence: u64,
    logs: VecDeque<Log>,
}
#[derive(Default)]
struct Controller {
    job: tokio::sync::Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::task::JoinHandle<()>,
        )>,
    >,
    activity: Mutex<Activity>,
    verbose: AtomicBool,
    log_file: Mutex<Option<File>>,
}
impl Controller {
    fn record(&self, level: &str, message: String, fields: BTreeMap<String, String>) {
        let mut activity = self.activity.lock().unwrap();
        if message == "update acknowledged" || message == "update applied and flushed" {
            activity.updates += 1;
            let mut bytes = 0;
            if let Some(text) = fields.get("bytes") {
                if let Ok(n) = text.parse::<u64>() {
                    bytes = n;
                }
            }
            activity.bytes += bytes;
        }
        if activity.running {
            if message == "caught up; live updates active"
                || message == "initial reconciliation complete"
            {
                activity.phase = "Live".to_string();
            } else if message == "client authenticated; reconciling files"
                || message == "connected and authenticated"
            {
                activity.phase = "Synchronizing".into();
            } else if message == "connection lost; will reconcile on reconnect" {
                activity.phase = "Reconnecting".into();
            } else if message == "relay listening" {
                activity.phase = "Listening".into();
            }
        }
        if level == "DEBUG" && !self.verbose.load(Ordering::Relaxed) {
            return;
        }
        activity.sequence += 1;
        let log = Log {
            id: activity.sequence,
            time: now(),
            level: level.into(),
            message,
            fields,
        };
        if let Some(file) = self.log_file.lock().unwrap().as_mut() {
            let _ = writeln!(
                file,
                "{} {} {} {:?}",
                log.time, log.level, log.message, log.fields
            );
        }
        activity.logs.push_back(log);
        while activity.logs.len() > 600 {
            activity.logs.pop_front();
        }
    }
    async fn stop(&self) {
        let mut job = self.job.lock().await;
        match job.take() {
            Some((stop, task)) => {
                self.activity.lock().unwrap().phase = "Stopping".into();
                let _ = stop.send(());
                let _ = task.await;
            }
            None => (),
        }
        let mut activity = self.activity.lock().unwrap();
        activity.running = false;
        activity.phase = "Stopped".into();
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
struct Visitor {
    message: String,
    fields: BTreeMap<String, String>,
}
impl tracing::field::Visit for Visitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let value = format!("{value:?}");
        if field.name() == "message" {
            self.message = value;
        } else {
            self.fields.insert(field.name().into(), value);
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.into();
        } else {
            self.fields.insert(field.name().into(), value.into());
        }
    }
}
struct Logs(Arc<Controller>);
impl<S: tracing::Subscriber> Layer<S> for Logs {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: LayerContext<'_, S>) {
        if !event.metadata().target().starts_with("elite_journal_relay") {
            return;
        }
        let mut visitor = Visitor {
            message: String::new(),
            fields: BTreeMap::new(),
        };
        event.record(&mut visitor);
        self.0.record(
            event.metadata().level().as_str(),
            visitor.message,
            visitor.fields,
        );
    }
}
struct Backend {
    controller: Arc<Controller>,
    settings: Mutex<Settings>,
    path: PathBuf,
}

#[derive(Serialize)]
struct Bootstrap {
    settings: Settings,
    profiles: Vec<locations::Profile>,
    platform: String,
    version: String,
    pair: String,
    config_path: PathBuf,
    addresses: Vec<String>,
    theme: Option<String>,
}
#[tauri::command]
async fn bootstrap(app: tauri::AppHandle, state: State<'_, Backend>) -> Result<Bootstrap, String> {
    let profiles = tauri::async_runtime::spawn_blocking(locations::discover)
        .await
        .map_err(|e| e.to_string())?;
    let mut settings = state.settings.lock().unwrap().clone();
    if settings.send.source.is_empty() {
        let mut sources = Vec::new();
        for profile in &profiles {
            if profile.has_journals {
                sources.push(profile.clone());
            }
        }
        if sources.len() == 1 {
            settings.send.source = sources[0].destination.to_string_lossy().into();
        }
        #[cfg(windows)]
        if let Ok(path) = files::default_destination() {
            settings.send.source = path.to_string_lossy().into();
        }
    }
    if settings.receive.destination.is_empty()
        && let Ok(path) = files::default_destination()
    {
        settings.receive.destination = path.to_string_lossy().into();
    }
    let mut theme = None;
    if let Some(window) = app.get_webview_window("main") {
        match window.theme() {
            Ok(value) => {
                let name = if value == tauri::Theme::Dark {
                    "dark"
                } else {
                    "light"
                };
                theme = Some(name.to_string());
            }
            Err(_) => (),
        }
    }
    Ok(Bootstrap {
        settings,
        profiles,
        platform: std::env::consts::OS.into(),
        version: env!("CARGO_PKG_VERSION").into(),
        pair: discovery::pair_id(&discovery::embedded_key()),
        config_path: state.path.clone(),
        addresses: discovery::local_addresses(),
        theme,
    })
}
#[tauri::command]
async fn scan_prefixes() -> Result<Vec<locations::Profile>, String> {
    tauri::async_runtime::spawn_blocking(locations::discover)
        .await
        .map_err(|e| e.to_string())
}
#[tauri::command]
async fn inspect_prefix(path: String) -> Result<Vec<locations::Profile>, String> {
    locations::profiles(&PathBuf::from(path)).map_err(|e| e.to_string())
}
#[tauri::command]
async fn pick_path(app: tauri::AppHandle, kind: String) -> Result<Option<String>, String> {
    let dialog = app.dialog().file();
    let selected = match kind.as_str() {
        "folder" | "prefix" => dialog.blocking_pick_folder(),
        "key" => dialog
            .add_filter("Pairing key", &["key", "txt"])
            .blocking_pick_file(),
        "log" => dialog.set_file_name("relay.log").blocking_save_file(),
        "certificate" | "private-key" => dialog
            .add_filter("TLS PEM certificate/key", &["pem", "crt", "key"])
            .blocking_pick_file(),
        _ => return Err("Unknown picker type".into()),
    };
    match selected {
        Some(file) => {
            let path = file.into_path().map_err(|e| e.to_string())?;
            Ok(Some(path.to_string_lossy().into_owned()))
        }
        None => Ok(None),
    }
}

#[tauri::command]
async fn save_settings(state: State<'_, Backend>, settings: Settings) -> Result<(), String> {
    let dir = state
        .path
        .parent()
        .context("config parent")
        .map_err(|e| e.to_string())?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut temp = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
    let data = serde_json::to_vec_pretty(&settings).map_err(|e| e.to_string())?;
    temp.write_all(&data).map_err(|e| e.to_string())?;
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    temp.persist(&state.path).map_err(|e| e.error.to_string())?;
    *state.settings.lock().unwrap() = settings.clone();
    Ok(())
}
#[tauri::command]
async fn create_key(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let Some(path) = app
        .dialog()
        .file()
        .set_file_name("relay.key")
        .blocking_save_file()
    else {
        return Ok(None);
    };
    let path = path.into_path().map_err(|e| e.to_string())?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|e| format!("Couldn't create key (it may already exist): {e}"))?;
    for byte in protocol::random_bytes().map_err(|e| e.to_string())? {
        write!(file, "{byte:02x}").map_err(|e| e.to_string())?;
    }
    file.sync_all().map_err(|e| e.to_string())?;
    Ok(Some(path.to_string_lossy().into()))
}
#[derive(Serialize)]
struct TlsFiles {
    certificate: String,
    key: String,
}
#[tauri::command]
async fn create_tls(app: tauri::AppHandle, name: String) -> Result<Option<TlsFiles>, String> {
    let Some(parent) = app.dialog().file().blocking_pick_folder() else {
        return Ok(None);
    };
    let directory = parent
        .into_path()
        .map_err(|e| e.to_string())?
        .join(format!("relay-tls-{}", now()));
    elite_journal_relay::transport::generate(&directory, &name).map_err(|e| e.to_string())?;
    Ok(Some(TlsFiles {
        certificate: directory.join("server-cert.pem").to_string_lossy().into(),
        key: directory.join("server-key.pem").to_string_lossy().into(),
    }))
}
#[derive(Serialize)]
struct Snapshot {
    running: bool,
    phase: String,
    error: Option<String>,
    updates: u64,
    bytes: u64,
    logs: Vec<Log>,
}
#[tauri::command]
fn snapshot(state: State<'_, Backend>, after: u64) -> Snapshot {
    let a = state.controller.activity.lock().unwrap();
    let mut logs = Vec::new();
    for log in &a.logs {
        if log.id > after {
            logs.push(log.clone());
        }
    }
    Snapshot {
        running: a.running,
        phase: a.phase.clone(),
        error: a.error.clone(),
        updates: a.updates,
        bytes: a.bytes,
        logs,
    }
}
#[tauri::command]
async fn stop_relay(state: State<'_, Backend>) -> Result<(), String> {
    state.controller.stop().await;
    Ok(())
}
#[tauri::command]
async fn start_relay(state: State<'_, Backend>, settings: Settings) -> Result<(), String> {
    let controller = state.controller.clone();
    let mut job = controller.job.lock().await;
    if let Some(active) = job.as_ref()
        && !active.1.is_finished()
    {
        return Err("Stop the active relay first".into());
    }
    let options = match settings.role {
        Role::Send => &settings.send,
        Role::Receive => &settings.receive,
    }
    .clone();
    let key = if options.key_file.trim().is_empty() {
        discovery::embedded_key()
    } else {
        protocol::load_key(&PathBuf::from(&options.key_file)).map_err(|e| e.to_string())?
    };
    if options.reconcile_ms < 10 || options.verify_secs < 1 {
        return Err("Scan must be at least 10 ms; verification at least 1 second".into());
    }
    match settings.role {
        Role::Send => {
            if !PathBuf::from(&options.source).is_dir() {
                return Err("Choose an existing journal source directory".into());
            }
        }
        Role::Receive => {
            if options.destination.trim().is_empty() || options.connect.trim().is_empty() {
                return Err("Choose a destination and a remote (or auto)".into());
            }
        }
    }
    let mode = match options.snapshot_mode.as_str() {
        "overwrite" => SnapshotMode::Overwrite,
        "atomic" => SnapshotMode::Atomic,
        _ => return Err("Invalid snapshot mode".into()),
    };
    if !(16..=4096).contains(&options.journal_chunk_kib) {
        return Err("Journal chunk size must be 16–4096 KiB".into());
    }
    let server_tls = if settings.role == Role::Send && options.tls {
        Some(
            elite_journal_relay::transport::server(
                &PathBuf::from(&options.tls_cert),
                &PathBuf::from(&options.tls_key),
            )
            .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };
    let client_tls = if settings.role == Role::Receive && options.tls {
        if options.connect == "auto" {
            return Err("TLS needs a remote address and a matching certificate name".into());
        }
        let ca = if options.tls_ca.is_empty() {
            None
        } else {
            Some(PathBuf::from(&options.tls_ca))
        };
        Some(
            elite_journal_relay::transport::client(ca.as_deref(), &options.tls_server_name)
                .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };
    let file = if options.log_file.trim().is_empty() {
        None
    } else {
        Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&options.log_file)
                .map_err(|e| e.to_string())?,
        )
    };
    let dir = state
        .path
        .parent()
        .context("config parent")
        .map_err(|e| e.to_string())?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut temp = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
    let data = serde_json::to_vec_pretty(&settings).map_err(|e| e.to_string())?;
    temp.write_all(&data).map_err(|e| e.to_string())?;
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    temp.persist(&state.path).map_err(|e| e.error.to_string())?;
    *state.settings.lock().unwrap() = settings.clone();
    *controller.log_file.lock().unwrap() = file;
    controller.verbose.store(options.verbose, Ordering::Relaxed);
    {
        let mut a = controller.activity.lock().unwrap();
        a.running = true;
        a.phase = "Starting".into();
        a.error = None;
        a.updates = 0;
        a.bytes = 0;
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let runtime = controller.clone();
    let task = tokio::spawn(async move {
        let shutdown = async {
            let _ = receiver.await;
        };
        let result = match settings.role {
            Role::Send => {
                app::serve_transport_until(
                    ServeOptions {
                        source: options.source.into(),
                        listen: options.listen,
                        key,
                        reconcile: Duration::from_millis(options.reconcile_ms),
                        verify: Duration::from_secs(options.verify_secs),
                    },
                    options.discovery,
                    elite_journal_relay::transport::ServerTransport {
                        tls: server_tls,
                        journal_chunk: Some(options.journal_chunk_kib as usize * 1024),
                    },
                    shutdown,
                )
                .await
            }
            Role::Receive => {
                app::receive_transport_until(
                    ReceiveOptions {
                        destination: options.destination.into(),
                        connect: options.connect,
                        key,
                        mode,
                        once: options.once,
                    },
                    client_tls,
                    shutdown,
                )
                .await
            }
        };
        if let Err(ref error) = result {
            runtime.record(
                "ERROR",
                format!("Relay stopped: {error:#}"),
                BTreeMap::new(),
            );
        }
        let mut a = runtime.activity.lock().unwrap();
        a.running = false;
        match result {
            Ok(()) => {
                a.phase = "Stopped".to_string();
                a.error = None;
            }
            Err(error) => {
                a.phase = "Failed".into();
                a.error = Some(format!("{error:#}"));
            }
        }
    });
    *job = Some((sender, task));
    Ok(())
}

fn main() {
    let controller = Arc::new(Controller::default());
    tracing_subscriber::registry()
        .with(Logs(controller.clone()))
        .init();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::ThemeChanged(theme) = event {
                let value = if *theme == tauri::Theme::Dark {
                    "dark"
                } else {
                    "light"
                };
                let _ = window.emit("system-theme", value);
            }
        })
        .setup(move |app| {
            let path = app.path().app_config_dir()?.join("settings.json");
            let settings = match fs::read(&path) {
                Ok(bytes) => match serde_json::from_slice(&bytes) {
                    Ok(settings) => settings,
                    Err(error) => {
                        controller.record(
                            "WARN",
                            format!("Invalid saved settings; using defaults: {error}"),
                            BTreeMap::new(),
                        );
                        Settings::default()
                    }
                },
                Err(_) => Settings::default(),
            };
            app.manage(Backend {
                controller,
                settings: Mutex::new(settings),
                path,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            scan_prefixes,
            inspect_prefix,
            pick_path,
            save_settings,
            create_key,
            create_tls,
            snapshot,
            start_relay,
            stop_relay
        ])
        .build(tauri::generate_context!())
        .expect("build desktop application")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event
                && code.is_none()
            {
                api.prevent_exit();
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    app.state::<Backend>().controller.stop().await;
                    app.exit(0);
                });
            }
        });
}
