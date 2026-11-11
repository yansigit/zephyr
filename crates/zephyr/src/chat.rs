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
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

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
            // One failed accept (EMFILE, a peer gone already) must not end the gateway.
            let accepted = listener.accept().await;
            let cred = accepted.and_then(|(stream, _)| Ok((stream.peer_cred()?, stream)));
            let (uid, stream) = match cred {
                Ok((cred, stream)) => (cred.uid().to_string(), stream),
                Err(e) => {
                    eprintln!("zephyr: chat accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            };
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
                    take_card(id, &v);
                    let m = Inbound {
                        channel: NAME.into(),
                        platform_msg_id: id.into(),
                        sender: uid.clone(),
                        text: text.into(),
                        derived: None,
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
    /// so the outbox keeps the reply for the next connection. A connection that does not take
    /// the line within `WRITE_TIMEOUT` (a client that stopped reading) is dropped.
    async fn send(&self, msg: &Outbound) -> anyhow::Result<()> {
        // A terminal cannot show a file: it gets the file's name.
        let text = match msg.attachment.as_deref().and_then(Path::file_name) {
            Some(name) => format!("{}\n[file] {}", msg.text, name.to_string_lossy()),
            None => msg.text.clone(),
        };
        // `re`: the message this answers (null for a notice), so `ask` skips other lines.
        let line = json!({"id": msg.client_id, "text": text, "re": msg.reply_to});
        let mut line = serde_json::to_vec(&line)?;
        line.push(b'\n');
        let mut delivered = false;
        let mut conns = self.conns.lock().await;
        let mut dead = Vec::new();
        for (id, (_, w)) in conns
            .iter_mut()
            .filter(|(_, (uid, _))| *uid == msg.recipient)
        {
            match tokio::time::timeout(WRITE_TIMEOUT, w.write_all(&line)).await {
                Ok(Ok(())) => delivered = true,
                _ => dead.push(*id),
            }
        }
        for id in dead {
            conns.remove(&id);
        }
        if !delivered {
            bail!("no chat client connected for {}", msg.recipient);
        }
        Ok(())
    }
}

/// A chat line's `card` (`{"number", "expiry", "cvc"}`, DESIGN §4.22), from `zephyr card put`:
/// kept in memory under the line's id for its `/card put` to claim (`cards::entered`), so the
/// number is never in the inbox row made of the line's text.
fn take_card(id: &str, line: &serde_json::Value) {
    let Some(card) = line.get("card") else { return };
    let field = |k: &str| card[k].as_str().unwrap_or_default();
    if let Ok(pan) = crate::cards::Pan::parse(field("number"), field("expiry"), field("cvc")) {
        crate::cards::enter(id, pan);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_card_rides_a_line_into_memory_and_not_into_the_row() {
        let dir = crate::test_dir("chat-card");
        let chat = Arc::new(ChatChannel::new(dir.join("chat.sock")));
        let (tx, mut rx) = mpsc::channel(1);
        let server = chat.clone();
        tokio::spawn(async move { server.run(tx).await });
        let mut client = loop {
            match UnixStream::connect(dir.join("chat.sock")).await {
                Ok(c) => break c,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            }
        };
        let card = json!({"number": "4111 1111 1111 1111", "expiry": "06/29", "cvc": "123"});
        let line = json!({"id": "chat-card-1", "text": "/card put 7 new", "card": card});
        client
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        let m = rx.recv().await.unwrap();
        assert_eq!(
            (m.platform_msg_id.as_str(), m.text.as_str()),
            ("chat-card-1", "/card put 7 new")
        );
        assert!(!format!("{m:?}").contains("4111"));
        assert_eq!(
            crate::cards::entered("chat-card-1").unwrap().last4(),
            "1111"
        );
    }

    #[tokio::test]
    async fn a_client_that_stops_reading_is_dropped() {
        let dir = crate::test_dir("chat-stuck");
        let chat = Arc::new(ChatChannel::new(dir.join("chat.sock")));
        let (tx, _rx) = mpsc::channel(1);
        let server = chat.clone();
        tokio::spawn(async move { server.run(tx).await });
        let client = loop {
            match UnixStream::connect(dir.join("chat.sock")).await {
                Ok(c) => break c,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            }
        };
        while chat.conns.lock().await.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let msg = Outbound {
            channel: NAME.into(),
            recipient: unsafe { libc::getuid() }.to_string(),
            client_id: "z1".into(),
            text: "x".repeat(4 << 20), // more than the socket buffers hold
            attachment: None,
            reply_to: None,
        };
        let sent = tokio::time::timeout(std::time::Duration::from_secs(5), chat.send(&msg)).await;
        assert!(sent.expect("send returned").is_err(), "nothing was read");
        assert!(
            chat.conns.lock().await.is_empty(),
            "the stuck client was dropped"
        );
        drop(client);
    }
}
