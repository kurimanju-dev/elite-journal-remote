//! Optional validated TLS; never opportunistically downgrades to plaintext.
use crate::protocol::{Channel, Digest};
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, OpenOptions},
    io::{BufReader, Write},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::{net::TcpStream, time::timeout};
use tokio_rustls::{
    TlsAcceptor, TlsConnector,
    rustls::{
        self,
        pki_types::{CertificateDer, ServerName},
    },
};

#[derive(Clone)]
pub struct ClientTls {
    connector: TlsConnector,
    name: ServerName<'static>,
}
#[derive(Clone, Default)]
pub struct ServerTransport {
    pub tls: Option<TlsAcceptor>,
    pub journal_chunk: Option<usize>,
}

fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let file = fs::File::open(path).context("read TLS certificate")?;
    let mut certs = Vec::new();
    let mut reader = BufReader::new(file);
    for cert in rustls_pemfile::certs(&mut reader) {
        certs.push(cert?);
    }
    ensure!(
        !certs.is_empty(),
        "TLS certificate file contains no PEM certificates"
    );
    Ok(certs)
}
pub fn server(cert: &Path, key: &Path) -> Result<TlsAcceptor> {
    let key_file = fs::File::open(key).context("read TLS private key")?;
    let mut reader = BufReader::new(key_file);
    let parsed = rustls_pemfile::private_key(&mut reader)?;
    let key = match parsed {
        Some(key) => key,
        None => anyhow::bail!("TLS key file contains no private key"),
    };
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(certificates(cert)?, key)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}
pub fn client(ca: Option<&Path>, name: &str) -> Result<ClientTls> {
    let mut roots = rustls::RootCertStore::empty();
    if let Some(ca) = ca {
        for certificate in certificates(ca)? {
            roots.add(certificate)?;
        }
    } else {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(ClientTls {
        connector: TlsConnector::from(Arc::new(config)),
        name: ServerName::try_from(name.to_owned()).context("invalid TLS server name")?,
    })
}
pub fn configure(stream: &TcpStream) -> Result<()> {
    stream.set_nodelay(true)?;
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10));
    if let Err(error) = socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive) {
        tracing::debug!(%error, "TCP keepalive unavailable; application heartbeat remains active");
    }
    Ok(())
}
pub async fn accept(stream: TcpStream, key: Digest, tls: Option<TlsAcceptor>) -> Result<Channel> {
    configure(&stream)?;
    if let Some(tls) = tls {
        let stream = timeout(Duration::from_secs(10), tls.accept(stream))
            .await
            .context("TLS handshake timed out")??;
        Channel::authenticate(stream, key, true).await
    } else {
        Channel::authenticate(stream, key, true).await
    }
}
pub async fn connect(stream: TcpStream, key: Digest, tls: Option<ClientTls>) -> Result<Channel> {
    configure(&stream)?;
    if let Some(tls) = tls {
        let stream = timeout(
            Duration::from_secs(10),
            tls.connector.connect(tls.name, stream),
        )
        .await
        .context("TLS handshake timed out")??;
        Channel::authenticate(stream, key, false).await
    } else {
        Channel::authenticate(stream, key, false).await
    }
}
pub fn generate(directory: &Path, name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty(),
        "specify a TLS DNS name or IP address"
    );
    let identity = rcgen::generate_simple_self_signed(vec![name.to_string()])?;
    let options = &mut fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        options.mode(0o700);
    }
    options
        .create(directory)
        .context("create new TLS directory (must not already exist)")?;
    let cert = identity.cert.pem();
    let key = identity.signing_key.serialize_pem();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(directory.join("server-cert.pem"))?;
    file.write_all(cert.as_bytes())?;
    file.sync_all()?;
    drop(file);

    let mut file = options.open(directory.join("server-key.pem"))?;
    file.write_all(key.as_bytes())?;
    file.sync_all()?;
    Ok(())
}
