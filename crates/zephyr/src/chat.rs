//! The M0 terminal channel: JSON lines over a unix socket. The sender is the
//! connecting process's uid, so `owners.chat` lists uids.

use crate::channel::{Channel, Inbound, Outbound};
use anyhow::{bail, Context};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex};

pub const NAME: &str = "chat";

type Conns = Arc<Mutex<HashMap<u64, (String, OwnedWriteHalf)>>>;

pub struct ChatChannel {
    path: PathBuf,
    conns: Conns,
}

impl ChatChannel {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            conns: Arc::default(),
        }
    }
}

impl Channel for ChatChannel {
    fn name(&self) -> &str {
        NAME
    }

    async fn run(&self, tx: mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        let _ = std::fs::remove_file(&self.path);
        let listener = UnixListener::bind(&self.path)
            .with_context(|| format!("binding {}", self.path.display()))?;
        for conn_id in 0u64.. {
            let (stream, _) = listener.accept().await?;
            let uid = stream.peer_cred()?.uid().to_string();
            let (read, write) = stream.into_split();
            self.conns
                .lock()
                .await
                .insert(conn_id, (uid.clone(), write));
            let (tx, conns) = (tx.clone(), self.conns.clone());
            tokio::spawn(async move {
                let mut lines = BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    // {"id": "<client message id>", "text": "..."}
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                        continue;
                    };
                    let (Some(id), Some(text)) = (v["id"].as_str(), v["text"].as_str()) else {
                        continue;
                    };
                    let m = Inbound {
                        channel: NAME.into(),
                        platform_msg_id: id.into(),
                        sender: uid.clone(),
                        text: text.into(),
                    };
                    if tx.send(m).await.is_err() {
                        break;
                    }
                }
                conns.lock().await.remove(&conn_id);
            });
        }
        Ok(())
    }

    /// Writes to every open connection of the recipient uid; errors when none is open
    /// so the outbox keeps the reply for the next connection.
    async fn send(&self, msg: &Outbound) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(&json!({"id": msg.client_id, "text": msg.text}))?;
        line.push(b'\n');
        let mut delivered = false;
        for (uid, w) in self.conns.lock().await.values_mut() {
            if *uid == msg.recipient && w.write_all(&line).await.is_ok() {
                delivered = true;
            }
        }
        if !delivered {
            bail!("no chat client connected for {}", msg.recipient);
        }
        Ok(())
    }
}

/// `zephyr chat`: stdin lines out, replies printed as they arrive.
pub async fn client(path: &Path) -> anyhow::Result<()> {
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting to {} (is zephyr running?)", path.display()))?;
    let (read, mut write) = stream.into_split();
    tokio::spawn(async move {
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let v: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
            println!("< {}", v["text"].as_str().unwrap_or(&line));
        }
        eprintln!("zephyr: connection closed");
        std::process::exit(0);
    });
    let session = format!("{}-{}", std::process::id(), crate::unix_millis());
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut n = 0;
    while let Some(text) = stdin.next_line().await? {
        n += 1;
        let mut line = serde_json::to_vec(&json!({"id": format!("{session}-{n}"), "text": text}))?;
        line.push(b'\n');
        write.write_all(&line).await?;
    }
    Ok(())
}
