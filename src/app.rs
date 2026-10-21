use crate::{
    files::{Destination, SnapshotMode, inventory as build_inventory},
    protocol::{Digest, Message},
    source::{Source, Update},
    watcher::Signals,
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    time::{sleep, timeout},
};

#[derive(Clone)]
pub struct ServeOptions {
    pub source: PathBuf,
    pub listen: String,
    pub key: Digest,
    pub reconcile: Duration,
    pub verify: Duration,
}

#[derive(Clone)]
pub struct ReceiveOptions {
    pub destination: PathBuf,
    pub connect: String,
    pub key: Digest,
    pub mode: SnapshotMode,
    pub once: bool,
}

pub async fn serve(options: ServeOptions, automatic_discovery: bool) -> Result<()> {
    serve_until(options, automatic_discovery, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

pub async fn serve_until(
    options: ServeOptions,
    automatic_discovery: bool,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<()> {
    serve_transport_until(
        options,
        automatic_discovery,
        crate::transport::ServerTransport::default(),
        shutdown,
    )
    .await
}

pub async fn serve_transport_until(
    options: ServeOptions,
    automatic_discovery: bool,
    transport: crate::transport::ServerTransport,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<()> {
    tokio::pin!(shutdown);
    ensure!(
        !options.reconcile.is_zero() && !options.verify.is_zero(),
        "scan intervals must be positive"
    );
    let source = options.source.canonicalize().context("source directory")?;
    ensure!(source.is_dir(), "source must be a directory");
    let listener = TcpListener::bind(&options.listen)
        .await
        .context("bind relay listener")?;
    let _advertisement = if automatic_discovery && transport.tls.is_none() {
        match crate::discovery::advertise(options.key, listener.local_addr()?).await {
            Ok(advertisement) => Some(advertisement),
            Err(error) => {
                tracing::warn!(%error,"automatic discovery unavailable; direct IP fallback remains available");
                None
            }
        }
    } else {
        None
    };
    tracing::info!(listen = %listener.local_addr()?, source = %source.display(), tls = transport.tls.is_some(), "relay listening");
    let mut signals = Signals::new(&source);
    let mut sessions = tokio::task::JoinSet::new();
    let permits = Arc::new(Semaphore::new(4));
    let mut maintenance = tokio::time::interval(options.reconcile);
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                tracing::info!(%peer,"client connected; authenticating");
                let permit = match permits.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        drop(stream);
                        continue;
                    }
                };
                let mut options = options.clone();
                options.source = source.clone();
                let events = signals.rx.clone();
                let transport = transport.clone();
                sessions.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = serve_session(stream, options, events, transport).await {
                        tracing::warn!(%peer, error = %format!("{error:#}"), "client disconnected");
                    }
                });
            }
            _ = maintenance.tick() => {
                if let Err(error) = signals.refresh() { tracing::warn!(%error, "watch refresh deferred"); }
            }
            _ = signals.rx.changed() => {
                if let Err(error) = signals.refresh() { tracing::warn!(%error, "watch refresh deferred"); }
            }
            _ = &mut shutdown => { sessions.shutdown().await; return Ok(()); },
            _ = sessions.join_next(), if !sessions.is_empty() => (),
        }
    }
}

async fn serve_session(
    stream: TcpStream,
    options: ServeOptions,
    mut events: tokio::sync::watch::Receiver<u64>,
    transport: crate::transport::ServerTransport,
) -> Result<()> {
    // Windows can delay notifications until the game flushes its open handle.
    let reconcile = if cfg!(windows) {
        options.reconcile.min(Duration::from_millis(25))
    } else {
        options.reconcile
    };
    let mut channel = crate::transport::accept(stream, options.key, transport.tls).await?;
    let hello = channel.recv().await?;
    ensure!(hello.payload.is_empty(), "unexpected inventory payload");
    let Message::Inventory(inventory) = hello.message else {
        bail!("expected inventory");
    };
    let mut source = Source::new(options.source, inventory)?;
    let chunk = match transport.journal_chunk {
        Some(bytes) => bytes,
        None => crate::source::CHUNK,
    };
    source = source.with_chunk_size(chunk)?;
    tracing::info!("client authenticated; reconciling files");
    let mut last_verify = Instant::now();
    let mut last_heartbeat = Instant::now();
    let mut id = 0u64;
    let mut announced = false;
    let mut updates_total = 0u64;
    let mut bytes_total = 0u64;
    let mut last_status = Instant::now();
    loop {
        let force = last_verify.elapsed() >= options.verify;
        if force {
            last_verify = Instant::now();
            channel.send(&Message::Rescan, &[]).await?;
            let inventory = channel.recv().await?;
            ensure!(inventory.payload.is_empty(), "unexpected rescan payload");
            let Message::Inventory(inventory) = inventory.message else {
                bail!("expected rescan inventory");
            };
            source.reconcile_inventory(inventory)?;
        }
        let (returned, updates) = tokio::task::spawn_blocking(move || {
            let updates = source.next_round(force);
            (source, updates)
        })
        .await?;
        source = returned;
        let updates = updates?;
        let has_updates = !updates.is_empty();
        for update in updates {
            id = id.checked_add(1).context("update IDs exhausted")?;
            let start = Instant::now();
            channel
                .send(
                    &Message::Update {
                        id,
                        name: update.name.clone(),
                        kind: update.kind,
                        offset: update.offset,
                        reset: update.reset,
                        prefix_hash: update.prefix_hash,
                        result: update.result.clone(),
                        created_unix_ms: update.created_unix_ms,
                    },
                    &update.data,
                )
                .await?;
            let ack = channel.recv().await?;
            match ack.message {
                Message::Ack { id: ack_id } => {
                    ensure!(ack_id == id && ack.payload.is_empty(), "invalid ACK");
                }
                _ => bail!("invalid ACK"),
            }
            source.acknowledge(&update);
            updates_total += 1;
            bytes_total += update.data.len() as u64;
            tracing::debug!(file = %update.name, bytes = update.data.len(), ack_ms = start.elapsed().as_secs_f64() * 1000.0, "update acknowledged");
            last_heartbeat = Instant::now();
        }
        if !source.pending && !announced {
            channel.send(&Message::CaughtUp, &[]).await?;
            tracing::info!("initial reconciliation complete");
            announced = true;
        }
        // Keep backfill moving, but wait if the game left an unfinished record.
        if source.pending && has_updates {
            continue;
        }
        if last_status.elapsed() >= Duration::from_secs(10) {
            tracing::info!(
                updates = updates_total,
                bytes = bytes_total,
                backfill_pending = source.pending,
                "client link active; files synchronized"
            );
            last_status = Instant::now();
        }
        if last_heartbeat.elapsed() >= Duration::from_secs(2) {
            channel.send(&Message::Ping, &[]).await?;
            let pong = channel.recv().await?;
            ensure!(
                matches!(pong.message, Message::Pong) && pong.payload.is_empty(),
                "expected pong"
            );
            last_heartbeat = Instant::now();
        }
        tokio::select! {
            _ = events.changed() => (),
            _ = sleep(reconcile) => (),
        }
    }
}

pub async fn receive(options: ReceiveOptions) -> Result<()> {
    receive_until(options, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

pub async fn receive_until(
    options: ReceiveOptions,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<()> {
    receive_transport_until(options, None, shutdown).await
}

pub async fn receive_transport_until(
    options: ReceiveOptions,
    tls: Option<crate::transport::ClientTls>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<()> {
    ensure!(
        tls.is_none() || options.connect != "auto",
        "TLS needs an explicit hostname:port or IP:port; discovery is local-network plaintext only"
    );
    tokio::pin!(shutdown);
    let writes = Arc::new(Semaphore::new(1));
    let res = async {
        let destination = Arc::new(Destination::open(&options.destination, options.mode)?);
        tracing::info!(destination = %destination.root.display(), server = %options.connect, "receiver starting");
        let mut backoff = Duration::from_millis(200);
        loop {
            let started = Instant::now();
            let session = receive_session(&options, destination.clone(), writes.clone(), tls.clone());
            let outcome = tokio::select! {
                outcome = session => outcome,
                _ = &mut shutdown => return Ok(()),
            };
            if options.once {
                return outcome;
            }
            if let Err(error) = outcome {
                tracing::warn!(error = %format!("{error:#}"), retry_ms = backoff.as_millis(), "connection lost; will reconcile on reconnect");
            }
            if started.elapsed() > Duration::from_secs(10) {
                backoff = Duration::from_millis(200);
            }
            tokio::select! {
                _ = sleep(backoff) => (),
                _ = &mut shutdown => return Ok(()),
            }
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }.await;
    // A new relay must not start while an old disk write is still running.
    let _finished = writes.acquire().await;
    res
}

async fn receive_session(
    options: &ReceiveOptions,
    destination: Arc<Destination>,
    writes: Arc<Semaphore>,
    tls: Option<crate::transport::ClientTls>,
) -> Result<()> {
    let mut channel = if options.connect == "auto" {
        crate::discovery::connect_auto(options.key).await?
    } else {
        let stream = timeout(Duration::from_secs(5), TcpStream::connect(&options.connect))
            .await
            .context("connect timed out")??;
        crate::transport::connect(stream, options.key, tls).await?
    };
    let root = destination.root.clone();
    let inventory = tokio::task::spawn_blocking(move || build_inventory(&root)).await??;
    channel.send(&Message::Inventory(inventory), &[]).await?;
    tracing::info!("connected and authenticated");
    let mut updates_total = 0u64;
    let mut bytes_total = 0u64;
    let mut last_status = Instant::now();
    loop {
        let frame = channel.recv().await?;
        match frame.message {
            Message::Update {
                id,
                name,
                kind,
                offset,
                reset,
                prefix_hash,
                result,
                created_unix_ms,
            } => {
                let update = Update {
                    name,
                    kind,
                    offset,
                    reset,
                    prefix_hash,
                    result,
                    created_unix_ms,
                    data: frame.payload,
                };
                let root = destination.clone();
                let started = Instant::now();
                let (name, bytes) = (update.name.clone(), update.data.len());
                let permit = writes.clone().acquire_owned().await?;
                let applied = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    root.apply(&update)
                })
                .await?;
                if let Err(error) = applied {
                    let _ = channel
                        .send(&Message::Error(format!("apply {name}: {error:#}")), &[])
                        .await;
                    return Err(error.context(format!("apply {name}")));
                }
                channel.send(&Message::Ack { id }, &[]).await?;
                updates_total += 1;
                bytes_total += bytes as u64;
                tracing::debug!(file = %name, bytes, apply_ms = started.elapsed().as_secs_f64() * 1000.0, "update applied and flushed");
            }
            Message::CaughtUp => {
                ensure!(frame.payload.is_empty(), "unexpected caught-up payload");
                tracing::info!("caught up; live updates active");
                if options.once {
                    return Ok(());
                }
            }
            Message::Ping => {
                ensure!(frame.payload.is_empty(), "unexpected ping payload");
                channel.send(&Message::Pong, &[]).await?;
                if last_status.elapsed() >= Duration::from_secs(10) {
                    tracing::info!(
                        updates = updates_total,
                        bytes = bytes_total,
                        "live; server connected and mirror healthy"
                    );
                    last_status = Instant::now();
                }
            }
            Message::Rescan => {
                ensure!(frame.payload.is_empty(), "unexpected rescan payload");
                let root = destination.root.clone();
                let inventory =
                    tokio::task::spawn_blocking(move || build_inventory(&root)).await??;
                channel.send(&Message::Inventory(inventory), &[]).await?;
            }
            _ => bail!("unexpected server message"),
        }
    }
}
