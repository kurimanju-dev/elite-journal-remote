//! Shared-key authentication and signed frames. This does not encrypt contents.
//! Use TLS, a trusted VM network, or an encrypted tunnel.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};

pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
const MAX_META: usize = 2 * 1024 * 1024;
const MAX_FRAME: usize = MAX_PAYLOAD + MAX_META + 4;
const MAGIC: &[u8; 8] = b"EDRELAY1";
const IO_TIMEOUT: Duration = Duration::from_secs(30);
pub type Digest = [u8; 32];
pub type Inventory = BTreeMap<String, FileState>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileState {
    pub len: u64,
    pub hash: Digest,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Kind {
    Journal,
    Snapshot,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Message {
    Inventory(Inventory),
    Update {
        id: u64,
        name: String,
        kind: Kind,
        offset: u64,
        reset: bool,
        prefix_hash: Digest,
        result: FileState,
        created_unix_ms: u64,
    },
    Ack {
        id: u64,
    },
    CaughtUp,
    Rescan,
    Ping,
    Pong,
    Error(String),
}

#[derive(Debug)]
pub struct Frame {
    pub message: Message,
    pub payload: Vec<u8>,
}

pub fn digest(bytes: &[u8]) -> Digest {
    *blake3::hash(bytes).as_bytes()
}

pub fn random_bytes() -> Result<Digest> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("OS randomness: {e}"))?;
    Ok(bytes)
}

pub fn load_key(path: &Path) -> Result<Digest> {
    let text = std::fs::read_to_string(path).context("read key file")?;
    let text = text.trim();
    ensure!(
        text.len() >= 32 && text.len() <= 4096,
        "key must contain 32–4096 characters"
    );
    Ok(blake3::derive_key(
        "elite-journal-relay v1 pre-shared key",
        text.as_bytes(),
    ))
}

fn mac(key: &Digest, parts: &[&[u8]]) -> Digest {
    let mut hasher = blake3::Hasher::new_keyed(key);
    for part in parts {
        hasher.update(part);
    }
    *hasher.finalize().as_bytes()
}

pub trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Stream for T {}

pub struct Channel {
    stream: Box<dyn Stream>,
    tx_key: Digest,
    rx_key: Digest,
    tx_seq: u64,
    rx_seq: u64,
}

impl Channel {
    pub async fn authenticate(
        stream: impl Stream + 'static,
        key: Digest,
        server: bool,
    ) -> Result<Self> {
        let mut stream: Box<dyn Stream> = Box::new(stream);
        let (s, c) = timeout(Duration::from_secs(10), async {
            let mut s = [0; 32];
            let mut c = [0; 32];
            if server {
                s = random_bytes()?;
                stream.write_all(MAGIC).await?;
                stream.write_all(&s).await?;
                stream.flush().await?;
                stream.read_exact(&mut c).await?;
                let mut proof = [0; 32];
                stream.read_exact(&mut proof).await?;
                let expected = mac(&key, &[b"client", &s, &c]);
                // Hash equality is constant-time.
                ensure!(
                    blake3::Hash::from(proof) == blake3::Hash::from(expected),
                    "authentication failed"
                );
                stream.write_all(&mac(&key, &[b"server", &s, &c])).await?;
                stream.flush().await?;
            } else {
                let mut magic = [0; 8];
                stream.read_exact(&mut magic).await?;
                ensure!(&magic == MAGIC, "unsupported relay protocol");
                stream.read_exact(&mut s).await?;
                c = random_bytes()?;
                stream.write_all(&c).await?;
                stream.write_all(&mac(&key, &[b"client", &s, &c])).await?;
                stream.flush().await?;
                let mut proof = [0; 32];
                stream.read_exact(&mut proof).await?;
                let expected = mac(&key, &[b"server", &s, &c]);
                ensure!(
                    blake3::Hash::from(proof) == blake3::Hash::from(expected),
                    "server authentication failed"
                );
            }
            Ok::<_, anyhow::Error>((s, c))
        })
        .await
        .context("authentication timed out")??;
        let s2c = mac(&key, &[b"server-to-client", &s, &c]);
        let c2s = mac(&key, &[b"client-to-server", &s, &c]);
        Ok(Self {
            stream,
            tx_key: if server { s2c } else { c2s },
            rx_key: if server { c2s } else { s2c },
            tx_seq: 0,
            rx_seq: 0,
        })
    }

    pub async fn send(&mut self, message: &Message, payload: &[u8]) -> Result<()> {
        let meta = serde_json::to_vec(message)?;
        ensure!(
            meta.len() <= MAX_META && payload.len() <= MAX_PAYLOAD,
            "frame too large"
        );
        // One write avoids lots of tiny packets on remote links.
        let body_len = 4 + meta.len() + payload.len();
        let len = (body_len as u32).to_be_bytes();
        let mut buf = Vec::with_capacity(36 + body_len);
        buf.extend_from_slice(&len);
        buf.extend_from_slice(&[0; 32]);
        buf.extend_from_slice(&(meta.len() as u32).to_be_bytes());
        buf.extend_from_slice(&meta);
        buf.extend_from_slice(payload);
        let tag = mac(
            &self.tx_key,
            &[&self.tx_seq.to_be_bytes(), &len, &buf[36..]],
        );
        buf[4..36].copy_from_slice(&tag);
        // If timed out/cancelled the caller MUST discard this connection.
        timeout(IO_TIMEOUT, async {
            self.stream.write_all(&buf).await?;
            self.stream.flush().await?;
            Ok::<_, std::io::Error>(())
        })
        .await
        .context("write timed out")??;
        match self.tx_seq.checked_add(1) {
            Some(next) => self.tx_seq = next,
            None => bail!("sequence exhausted"),
        }
        Ok(())
    }

    pub async fn recv(&mut self) -> Result<Frame> {
        let frame = timeout(IO_TIMEOUT, async {
            let len = self.stream.read_u32().await? as usize;
            ensure!((4..=MAX_FRAME).contains(&len), "invalid frame length");
            let mut tag = [0; 32];
            self.stream.read_exact(&mut tag).await?;
            let mut body = vec![0; len];
            self.stream.read_exact(&mut body).await?;
            let sequence = self.rx_seq.to_be_bytes();
            let length = (len as u32).to_be_bytes();
            let expected = mac(&self.rx_key, &[&sequence, &length, &body]);
            ensure!(
                blake3::Hash::from(tag) == blake3::Hash::from(expected),
                "frame authentication failed"
            );
            let header = [body[0], body[1], body[2], body[3]];
            let meta_len = u32::from_be_bytes(header) as usize;
            if meta_len > MAX_META || meta_len > len - 4 {
                bail!("invalid metadata length");
            }
            let payload = body.split_off(4 + meta_len);
            ensure!(payload.len() <= MAX_PAYLOAD, "payload too large");
            let message = serde_json::from_slice(&body[4..])?;
            Ok::<_, anyhow::Error>(Frame { message, payload })
        })
        .await
        .context("read timed out")??;
        if self.rx_seq == u64::MAX {
            bail!("sequence exhausted");
        }
        self.rx_seq += 1;
        if let Message::Error(message) = &frame.message {
            bail!("peer: {message}");
        }
        Ok(frame)
    }
}
