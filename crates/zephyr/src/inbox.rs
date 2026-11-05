//! Files the owner sends on Discord or iMessage (DESIGN §4.3). The adapter saves them as the
//! agent under `<home>/inbox/` before the row goes to the router, and appends one line per file
//! to the row's text, so the inbox schema, dedupe and reconcile stay as they are. Only an owner's
//! files are fetched: the adapter reads the live owner list, which a reload updates.

use crate::channel::Inbound;
use crate::config::Config;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::watch;

/// At most this many files of one message are fetched; the rest are only listed.
pub const MAX_FILES: usize = 10;
/// A larger file is listed but not fetched (Discord's upload limit for most accounts).
pub const MAX_BYTES: u64 = 25 * 1024 * 1024;
/// A fetch is tried this many times: an iMessage file may still be arriving on the Mac.
const TRIES: u32 = 3;
const RETRY_AFTER: Duration = Duration::from_secs(2);
/// The image types rpi's `read` shows the model.
pub(crate) const IMAGES: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// The router's live config, handed to the adapter once at start (`Channel::live`). Unset means
/// no owners, so nothing is fetched.
pub type Live = Arc<OnceLock<watch::Receiver<Arc<Config>>>>;

/// One attachment as the platform describes it, and the request that fetches it.
pub struct File {
    pub name: String,
    pub mime: Option<String>,
    /// The platform's size, `0` when it gives none.
    pub size: u64,
    pub req: reqwest::RequestBuilder,
}

/// Appends a line per file to `inbound.text`, saving each file first when the sender is an owner.
pub async fn append(live: &Live, inbound: &mut Inbound, files: Vec<File>) {
    let cfg = live.get().map(|rx| rx.borrow().clone());
    let dir = cfg
        .filter(|c| c.is_owner(&inbound.channel, &inbound.sender))
        .map(|c| c.home.join("inbox"));
    let id: String = inbound
        .platform_msg_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    for (i, f) in files.into_iter().enumerate() {
        let declared = f.mime.as_deref().unwrap_or("application/octet-stream");
        let not = |why: &str| {
            let kind = kind(declared.strip_prefix("image/").unwrap_or(""));
            format!(
                "[attached {kind}: {} ({declared}, {}, {why})]",
                f.name,
                size(f.size)
            )
        };
        let line = match &dir {
            None => not("not downloaded"),
            Some(_) if i >= MAX_FILES => not(&format!("over {MAX_FILES} files, not downloaded")),
            Some(_) if f.size > MAX_BYTES => not("too large, not downloaded"),
            Some(dir) => {
                let stem = format!("{}-{id}-{}", inbound.channel, i + 1);
                match save(dir, &stem, &f).await {
                    Ok((path, mime, len)) => {
                        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                        match kind(ext) {
                            "image" => attached_image_line(&path, &f.name, &mime, len),
                            _ => format!(
                                "[attached file: {} ({}, {mime}, {})]",
                                path.display(),
                                f.name,
                                size(len)
                            ),
                        }
                    }
                    Err(e) => {
                        eprintln!("zephyr: {} attachment {}: {e:#}", inbound.channel, i + 1);
                        not("download failed")
                    }
                }
            }
        };
        if !inbound.text.is_empty() {
            inbound.text.push('\n');
        }
        inbound.text += &line;
    }
}

/// The line that tells the model where an image is; the TUI's `@image` adds the same one.
pub(crate) fn attached_image_line(path: &Path, name: &str, mime: &str, len: u64) -> String {
    format!(
        "[attached image: {} ({name}, {mime}, {}); open it with read]",
        path.display(),
        size(len)
    )
}

fn kind(ext: &str) -> &'static str {
    if IMAGES.contains(&ext) {
        "image"
    } else {
        "file"
    }
}

/// `240 KB`, `1.2 MB`.
fn size(bytes: u64) -> String {
    if bytes < 1024 * 1024 {
        format!("{} KB", bytes.div_ceil(1024))
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Fetches `f` into `dir/<stem>.<ext>` as the agent and returns the path, its type and size. A
/// file saved under `stem` before (iMessage backfill re-reads the last day at every start) is
/// reused, not fetched again.
async fn save(dir: &Path, stem: &str, f: &File) -> anyhow::Result<(PathBuf, String, u64)> {
    if let Some(path) = existing(dir, stem) {
        let len = {
            let _fs = crate::agent::fs();
            std::fs::metadata(&path)?.len()
        };
        let mime = f.mime.clone().unwrap_or_default();
        return Ok((path, mime, len));
    }
    let (bytes, mime) = fetch(&f.req).await?;
    let mime = mime.or(f.mime.clone()).unwrap_or_default();
    // The type the platform served wins: BlueBubbles converts HEIC photos to JPEG on download.
    let ext = match mime.as_str() {
        "image/jpeg" => "jpg".to_string(),
        "image/png" | "image/gif" | "image/webp" => mime[6..].to_string(),
        _ => Path::new(&f.name)
            .extension()
            .and_then(|e| e.to_str())
            .filter(|e| e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()))
            .map_or("bin".into(), |e| e.to_ascii_lowercase()),
    };
    let _fs = crate::agent::fs();
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{stem}.{ext}"));
    // Written aside and renamed, so a half-written file is never taken for a saved one.
    let part = dir.join(format!(".{stem}.part"));
    std::fs::write(&part, &bytes)?;
    std::fs::rename(&part, &path)?;
    Ok((path, mime, bytes.len() as u64))
}

fn existing(dir: &Path, stem: &str) -> Option<PathBuf> {
    let _fs = crate::agent::fs();
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_stem().is_some_and(|s| s == stem))
}

/// The body (at most `MAX_BYTES`) and the served `Content-Type`. Errors carry no URL, which for
/// BlueBubbles holds the server password.
async fn fetch(req: &reqwest::RequestBuilder) -> anyhow::Result<(Vec<u8>, Option<String>)> {
    let mut tries = 0;
    let mut res = loop {
        tries += 1;
        let req = req.try_clone().expect("a GET has no body to clone");
        let err = match req.send().await {
            Ok(res) if res.status().is_success() => break res,
            Ok(res) => anyhow::anyhow!("HTTP {}", res.status()),
            Err(e) => anyhow::Error::new(e.without_url()),
        };
        if tries == TRIES {
            return Err(err.context(format!("download failed {TRIES} times")));
        }
        tokio::time::sleep(RETRY_AFTER).await;
    };
    let mime = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_string());
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(|e| e.without_url())? {
        body.extend_from_slice(&chunk);
        anyhow::ensure!(body.len() as u64 <= MAX_BYTES, "larger than 25 MB");
    }
    Ok((body, mime))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use http_body_util::Full;
    use hyper::body::{Bytes, Incoming};
    use hyper::{Request, Response};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    /// Serves `body` as `mime` for every request and reports each request's `path?query`.
    pub async fn file_stub(
        status: u16,
        mime: &'static str,
        body: &'static [u8],
    ) -> (String, mpsc::UnboundedReceiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                        let _ = tx.send(req.uri().to_string());
                        async move {
                            Ok::<_, std::convert::Infallible>(
                                Response::builder()
                                    .status(status)
                                    .header("content-type", mime)
                                    .body(Full::new(Bytes::from_static(body)))
                                    .unwrap(),
                            )
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        (format!("http://{addr}"), rx)
    }

    /// A live config with `home` and these owners, as the router hands it over.
    pub fn live(home: &Path, owners: &str) -> Live {
        let cfg: Config = toml::from_str(&format!(
            "rpi = '/r'\nhome = {home:?}\n[owners]\n{owners}\n"
        ))
        .unwrap();
        let live = Live::default();
        let _ = live.set(watch::channel(Arc::new(cfg)).1);
        live
    }

    #[test]
    fn sizes_read_like_a_person_wrote_them() {
        assert_eq!(size(0), "0 KB");
        assert_eq!(size(240 * 1024 - 5), "240 KB");
        assert_eq!(size(1_258_291), "1.2 MB");
    }

    #[tokio::test]
    async fn a_file_saved_before_is_not_fetched_again() {
        let home = crate::test_dir("inbox-reuse");
        let (base, mut seen) = file_stub(200, "image/jpeg", b"jpeg-bytes").await;
        let live = live(&home, "discord = ['2']");
        let file = || File {
            name: "IMG_1.HEIC".into(),
            mime: Some("image/heic".into()),
            size: 10,
            req: reqwest::Client::new().get(format!("{base}/a")),
        };
        let row = || Inbound {
            channel: "discord".into(),
            platform_msg_id: "7".into(),
            sender: "2".into(),
            text: String::new(),
        };
        let (mut first, mut again) = (row(), row());
        append(&live, &mut first, vec![file()]).await;
        append(&live, &mut again, vec![file()]).await;
        let path = home.join("inbox/discord-7-1.jpg");
        assert_eq!(
            first.text,
            format!(
                "[attached image: {} (IMG_1.HEIC, image/jpeg, 1 KB); open it with read]",
                path.display()
            )
        );
        assert!(again
            .text
            .starts_with(&format!("[attached image: {} ", path.display())));
        assert_eq!(std::fs::read(&path).unwrap(), b"jpeg-bytes");
        assert_eq!(seen.recv().await.unwrap(), "/a");
        assert!(seen.try_recv().is_err(), "fetched once");
    }

    #[tokio::test]
    async fn a_failed_download_is_listed_and_the_message_kept() {
        let home = crate::test_dir("inbox-fail");
        let (base, _seen) = file_stub(500, "text/plain", b"no").await;
        let mut row = Inbound {
            channel: "imessage".into(),
            platform_msg_id: "G".into(),
            sender: "+1".into(),
            text: "look".into(),
        };
        let f = File {
            name: "a.pdf".into(),
            mime: Some("application/pdf".into()),
            size: 2048,
            req: reqwest::Client::new().get(format!("{base}/x")),
        };
        append(&live(&home, "imessage = ['+1']"), &mut row, vec![f]).await;
        assert_eq!(
            row.text,
            "look\n[attached file: a.pdf (application/pdf, 2 KB, download failed)]"
        );
        assert!(existing(&home.join("inbox"), "imessage-G-1").is_none());
    }
}
