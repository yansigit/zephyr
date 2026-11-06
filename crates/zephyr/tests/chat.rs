//! The chat socket under fd exhaustion. A binary of its own: it lowers the process's fd limit.

use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use zephyr::channel::Channel;
use zephyr::chat::ChatChannel;

#[tokio::test]
async fn a_failed_accept_does_not_end_the_chat_channel() {
    let dir = std::env::temp_dir().join(format!("zephyr-chat-emfile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("chat.sock");
    let (tx, mut rx) = mpsc::channel(8);
    let chat = ChatChannel::new(sock.clone());
    let server = tokio::spawn(async move { chat.run(tx).await });
    while !sock.exists() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // One fd left: the client's socket takes it, so the server's accept fails with EMFILE.
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: plain syscalls on a struct we own.
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
    let open = std::fs::read_dir("/proc/self/fd").unwrap().count() as u64; // counts its own fd
    let low = libc::rlimit {
        rlim_cur: open,
        rlim_max: limit.rlim_max,
    };
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &low) };
    let mut client = tokio::net::UnixStream::connect(&sock).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await; // a few failed accepts
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
    assert!(
        !server.is_finished(),
        "the channel ended: {:?}",
        server.await
    );

    client
        .write_all(b"{\"id\": \"m1\", \"text\": \"still here\"}\n")
        .await
        .unwrap();
    let m = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the message arrived")
        .unwrap();
    assert_eq!(m.text, "still here");
}
