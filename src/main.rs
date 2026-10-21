use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use elite_journal_relay::{
    app::{self, ReceiveOptions, ServeOptions},
    files::{SnapshotMode, default_destination, default_source},
    protocol::{load_key, random_bytes},
};
use std::{fs::OpenOptions, io::Write, path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(version, about = "Send Elite Dangerous journals over TCP")]
struct Cli {
    /// Debug logs (or set RUST_LOG).
    #[arg(long, global = true)]
    verbose: bool,
    /// Append logs to this file.
    #[arg(long, global = true)]
    log_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a shared pairing key.
    Keygen {
        #[arg(long)]
        out: PathBuf,
    },
    /// Send journals without changing source files.
    Serve {
        /// Journal folder (auto-detected if omitted).
        #[arg(long)]
        source: Option<PathBuf>,
        /// Listen address.
        #[arg(long, default_value = "0.0.0.0:48933")]
        listen: String,
        /// Pairing key file (uses the embedded key if omitted).
        #[arg(long)]
        key_file: Option<PathBuf>,
        /// Disable LAN discovery.
        #[arg(long)]
        no_discovery: bool,
        /// TLS server certificate (requires --tls-key).
        #[arg(long, requires = "tls_key")]
        tls_cert: Option<PathBuf>,
        /// TLS private key (requires --tls-cert).
        #[arg(long, requires = "tls_cert")]
        tls_key: Option<PathBuf>,
        /// Backfill chunk size in KiB.
        #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u64).range(16..=4096))]
        journal_chunk_kib: u64,
        /// Fallback scan interval in milliseconds.
        #[arg(long, default_value_t = 250, value_parser = clap::value_parser!(u64).range(10..))]
        reconcile_ms: u64,
        /// Content check interval in seconds.
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
        verify_secs: u64,
    },
    /// Receive journals; reconnect if disconnected.
    Receive {
        /// Server address, or auto for LAN discovery.
        #[arg(long, default_value = "auto")]
        connect: String,
        /// Destination folder.
        #[arg(long)]
        destination: Option<PathBuf>,
        /// Wine prefix or Proton compatdata entry.
        #[arg(long, conflicts_with = "destination")]
        wine_prefix: Option<PathBuf>,
        /// Windows user in the prefix.
        #[arg(long, requires = "wine_prefix")]
        wine_user: Option<String>,
        /// Use TLS. There is no plaintext fallback.
        #[arg(long)]
        tls: bool,
        /// Certificate or CA to trust (uses public roots if omitted).
        #[arg(long, requires = "tls")]
        tls_ca: Option<PathBuf>,
        /// Expected certificate name (defaults to the connection address).
        #[arg(long, requires = "tls")]
        tls_server_name: Option<String>,
        /// Pairing key file (uses the embedded key if omitted).
        #[arg(long)]
        key_file: Option<PathBuf>,
        /// Snapshot write mode.
        #[arg(long, value_enum, default_value = "overwrite")]
        snapshot_mode: SnapshotMode,
        /// Stop after the initial sync.
        #[arg(long)]
        once: bool,
    },
    /// Show default folders and build architecture.
    Doctor,
    /// List detected Wine and game profiles as JSON.
    Prefixes,
    /// Create a TLS certificate and key in a new directory.
    TlsKeygen {
        /// New directory for the certificate and key.
        #[arg(long)]
        out_dir: PathBuf,
        /// Certificate DNS name or IP address.
        #[arg(long, default_value = "elite-relay")]
        name: String,
    },
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let filter = match tracing_subscriber::EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(_) => {
            let level = if cli.verbose {
                "elite_journal_relay=debug"
            } else {
                "elite_journal_relay=info"
            };
            tracing_subscriber::EnvFilter::new(level)
        }
    };
    let writer: tracing_subscriber::fmt::writer::BoxMakeWriter = if let Some(path) = &cli.log_file {
        tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::sync::Mutex::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .context("open log file")?,
        ))
    } else {
        tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stdout)
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .with_target(false)
        .init();
    match cli.command {
        Command::Keygen { out } => {
            let key = random_bytes()?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&out)
                .context("create key file (will not overwrite)")?;
            for byte in key {
                write!(file, "{byte:02x}")?;
            }
            writeln!(file)?;
            file.sync_all()?;
            println!(
                "Created {}. Keep it private and copy it to the other device.",
                out.display()
            );
        }
        Command::Serve {
            source,
            listen,
            key_file,
            reconcile_ms,
            verify_secs,
            no_discovery,
            tls_cert,
            tls_key,
            journal_chunk_kib,
        } => {
            let tls = match (tls_cert, tls_key) {
                (Some(cert), Some(key)) => {
                    Some(elite_journal_relay::transport::server(&cert, &key)?)
                }
                _ => None,
            };
            let source = match source {
                Some(path) => path,
                None => default_source()?,
            };
            let key = match key_file {
                Some(path) => load_key(&path)?,
                None => elite_journal_relay::discovery::embedded_key(),
            };
            let config = ServeOptions {
                source,
                listen,
                key,
                reconcile: Duration::from_millis(reconcile_ms),
                verify: Duration::from_secs(verify_secs),
            };
            let transport = elite_journal_relay::transport::ServerTransport {
                tls,
                journal_chunk: Some(journal_chunk_kib as usize * 1024),
            };
            app::serve_transport_until(config, !no_discovery, transport, async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        }
        Command::Receive {
            connect,
            destination,
            wine_prefix,
            wine_user,
            tls,
            tls_ca,
            tls_server_name,
            key_file,
            snapshot_mode,
            once,
        } => {
            let tls = if tls {
                anyhow::ensure!(
                    connect != "auto",
                    "TLS requires an explicit --connect hostname:port or IP:port"
                );
                let (host, _) = connect
                    .rsplit_once(':')
                    .context("remote address needs a port")?;
                let inferred = host.trim_matches(['[', ']']);
                let name = match tls_server_name.as_deref() {
                    Some(name) => name,
                    None => inferred,
                };
                let config = elite_journal_relay::transport::client(tls_ca.as_deref(), name)?;
                Some(config)
            } else {
                None
            };
            let path;
            if let Some(prefix) = wine_prefix {
                path = elite_journal_relay::locations::destination(&prefix, wine_user.as_deref())?;
            } else {
                path = match destination {
                    Some(path) => path,
                    None => default_destination()?,
                };
            }
            let key = match key_file {
                Some(path) => load_key(&path)?,
                None => elite_journal_relay::discovery::embedded_key(),
            };
            let config = ReceiveOptions {
                destination: path,
                connect,
                key,
                mode: snapshot_mode,
                once,
            };
            app::receive_transport_until(config, tls, async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        }
        Command::TlsKeygen { out_dir, name } => {
            elite_journal_relay::transport::generate(&out_dir, &name)?;
            println!(
                "Created TLS files in {}. Share only server-cert.pem; name: {}",
                out_dir.display(),
                name
            );
        }
        Command::Prefixes => {
            let profiles = elite_journal_relay::locations::discover();
            let json = serde_json::to_string_pretty(&profiles)?;
            println!("{json}");
        }
        Command::Doctor => {
            println!(
                "elite-journal-relay {} / {} / {}",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS,
                std::env::consts::ARCH
            );
            for (name, result) in [
                ("Default source", default_source()),
                ("Default destination", default_destination()),
            ] {
                match result {
                    Ok(path) => println!("{name}: {}", path.display()),
                    Err(error) => println!("{name}: {error}"),
                }
            }
        }
    }
    Ok(())
}
