//! The network floor against a real container (DESIGN §4.19): as the agent's uid nothing is
//! reachable but the daemon's proxy, and the proxy refuses what it must.
//!
//! Ignored: it needs Docker and a side container already up, made beside any live install with
//! its own name, image, data dirs and attach port:
//!
//! ```sh
//! export ZEPHYR_CONTAINER=zephyr-test ZEPHYR_IMAGE=zephyr-dev:$(git rev-parse --short HEAD)
//! export ZEPHYR_SRC=$PWD HOME=$(mktemp -d) && mkdir "$HOME/p"
//! zephyr init --non-interactive --no-start --projects "$HOME/p" --provider anthropic --model anthropic/claude-sonnet-5
//! zephyr config set attach.port 17790 && zephyr up --build
//! cargo test -p zephyr --test net_floor -- --ignored
//! ```
//!
//! `ZEPHYR_TEST_ATTACH_PORT` names another attach port than 17790.

use std::process::{Command, Output};
use std::time::{Duration, Instant};

use zephyr::agent::{browser_args, AGENT_PATH, PROXY_ENV};

/// curl's exit code for a connection that was refused; 28 would be a timeout.
const REFUSED: i32 = 7;

struct Side {
    container: String,
}

impl Side {
    fn root(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new("docker");
        cmd.args(["exec", &self.container]).args(args);
        cmd.output().expect("running docker")
    }

    /// Runs `args` as the agent, with what `agent::cmd` gives its processes in bridge mode.
    fn agent(&self, args: &[&str]) -> Output {
        // SAFETY: getuid and getgid have no arguments and cannot fail.
        let ids = unsafe { format!("{}:{}", libc::getuid(), libc::getgid()) };
        let image_args = text(&self.root(&["printenv", "AGENT_BROWSER_ARGS"]));
        let mut cmd = Command::new("docker");
        cmd.args(["exec", "-u", &ids, "-w", "/zephyr"]);
        cmd.args(["-e", &format!("PATH={AGENT_PATH}")]);
        for (key, value) in PROXY_ENV {
            cmd.args(["-e", &format!("{key}={value}")]);
        }
        let browser = browser_args(Some(image_args.trim().to_string()));
        cmd.args(["-e", &format!("AGENT_BROWSER_ARGS={browser}")]);
        cmd.arg(&self.container).args(args);
        cmd.output().expect("running docker")
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
#[ignore = "needs Docker and a side container (see the top of this file)"]
fn the_agent_reaches_only_the_proxy() {
    let container = std::env::var("ZEPHYR_CONTAINER").unwrap_or_default();
    assert!(
        !container.is_empty() && container != "zephyr",
        "set ZEPHYR_CONTAINER to a side container: this test never runs against the live install"
    );
    let docker = Command::new("docker").arg("version").output();
    if !docker.is_ok_and(|o| o.status.success()) {
        if std::env::var("ZEPHYR_REAL_RPI").as_deref() == Ok("1") {
            panic!("ZEPHYR_REAL_RPI=1 but Docker does not answer");
        }
        return eprintln!("skipping: Docker does not answer");
    }
    let port = std::env::var("ZEPHYR_TEST_ATTACH_PORT").unwrap_or_else(|_| "17790".into());
    let side = Side { container };
    let mode = Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{.HostConfig.NetworkMode}}",
            &side.container,
        ])
        .output()
        .unwrap();
    assert_eq!(text(&mode).trim(), side.container, "not on its own bridge");

    // Directly, everything is refused at once: zephyr's own ports on the container's loopback
    // (nothing listens there), the LAN, the tailnet, cloud metadata, the internet and the host.
    let gateway = Command::new("docker")
        .args([
            "network",
            "inspect",
            "-f",
            "{{(index .IPAM.Config 0).Gateway}}",
        ])
        .arg(&side.container)
        .output()
        .unwrap();
    let gateway = format!("http://{}:22/", text(&gateway).trim());
    let own = text(&side.root(&["getent", "hosts", "zephyr"]));
    let own = own
        .split_whitespace()
        .next()
        .expect("the container's address");
    let attach = format!("http://{own}:{port}/");
    for url in [
        format!("http://127.0.0.1:{port}/").as_str(),
        "http://127.0.0.1:7790/",
        attach.as_str(),
        "http://192.168.1.1/",
        "http://100.100.100.100/",
        "http://169.254.169.254/",
        "https://1.1.1.1",
        gateway.as_str(),
    ] {
        let started = Instant::now();
        let out = side.agent(&["curl", "-sS", "-m", "5", "--noproxy", "*", url]);
        assert_eq!(out.status.code(), Some(REFUSED), "{url}: {}", text(&out));
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{url} was not refused at once"
        );
    }
    // The same ports answer root, so the refusals above are the rules' and not a closed port.
    let root = side.root(&["curl", "-s", "-m", "5", "-o", "/dev/null", &attach]);
    assert_ne!(
        root.status.code(),
        Some(REFUSED),
        "the attach port is not up"
    );

    // Through the proxy, private and metadata addresses get 403, and no name resolves without it.
    for url in [
        "http://192.168.1.1/",
        "http://100.100.100.100/",
        "http://169.254.169.254/",
    ] {
        let out = side.agent(&["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}", url]);
        assert_eq!(text(&out), "403", "{url}");
    }
    let tunnel = side.agent(&["curl", "-sS", "-m", "5", "https://[::ffff:127.0.0.1]/"]);
    assert!(text(&tunnel).contains("403"), "{}", text(&tunnel));
    assert!(!side
        .agent(&["getent", "hosts", "example.com"])
        .status
        .success());

    // The internet works through the proxy, for curl and for both browsers.
    let page = side.agent(&["curl", "-sS", "-m", "20", "https://example.com"]);
    assert!(text(&page).contains("Example Domain"), "{}", text(&page));
    let opened = side.agent(&["agent-browser", "open", "https://example.com"]);
    assert!(opened.status.success(), "{}", text(&opened));
    let title = side.agent(&["agent-browser", "get", "title"]);
    assert!(text(&title).contains("Example Domain"), "{}", text(&title));
    side.agent(&["agent-browser", "close"]);
    const CAMOUFOX: &str = "from camoufox.sync_api import Camoufox\n\
        with Camoufox(headless=True) as browser:\n    page = browser.new_page()\n    \
        page.goto('https://example.com')\n    print(page.title())\n";
    let fox = side.agent(&["/opt/zephyr/camoufox/camoufox-python", "-c", CAMOUFOX]);
    assert!(text(&fox).contains("Example Domain"), "{}", text(&fox));

    // The audit lists where the agent went, which proves the browsers used the proxy, and what
    // was refused.
    const ROWS: &str = "select args || '|' || outcome from audit where session = 'net'";
    let audit = text(&side.root(&["sqlite3", "/var/lib/zephyr/state.db", ROWS]));
    let allowed = |l: &str| l.starts_with("example.com:443 -> ") && l.ends_with("|allowed");
    assert!(audit.lines().any(allowed), "{audit}");
    let refused = "169.254.169.254:80|refused: private address 169.254.169.254";
    assert!(audit.lines().any(|l| l == refused), "{audit}");

    // doctor proves the same, and the daemon cannot change the rules: neither NET_ADMIN (bit 12)
    // nor SETPCAP (bit 8) is in its effective set.
    let doctor = text(&side.root(&["zephyr", "doctor"]));
    for line in [
        "ok   a direct connection to the internet is refused",
        &format!("ok   the daemon's port {port} is refused"),
        "ok   no name resolves without the proxy",
        "ok   the proxy refuses 169.254.169.254:80",
        "ok   the proxy refuses [::ffff:7f00:1]:443",
        "ok   the proxy reaches example.com:443",
        "ok   nothing of root's listens where the agent can reach it",
    ] {
        assert!(doctor.contains(line), "{line}\n{doctor}");
    }
    assert!(!doctor.contains("network floor off"), "{doctor}");
    // The daemon alone, by its whole command line: `supervise` and this shell keep both.
    const DAEMON_CAPS: &str = "for d in /proc/[0-9]*; do \
        [ \"$(tr '\\0' ' ' < $d/cmdline 2>/dev/null)\" = '/usr/local/bin/zephyr run ' ] \
        && grep CapEff $d/status; done; true";
    let caps = text(&side.root(&["sh", "-c", DAEMON_CAPS]));
    let caps: Vec<&str> = caps.split_whitespace().collect();
    assert_eq!(caps.len(), 2, "one daemon: {caps:?}");
    let caps = u64::from_str_radix(caps[1], 16).unwrap();
    assert_eq!(caps & (1 << 12 | 1 << 8), 0, "CapEff {caps:x}");

    // And the owner still reaches the attach port, published on this machine's loopback.
    std::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .expect("the attach port is published");

    // network.allow opens a direct route, for programs that speak no proxy: ssh's greeting from
    // a listed host and port, by name, with no resolver for anything else.
    let greeting = |host: &str, port: u16| {
        let open = format!("exec 3<>/dev/tcp/{host}/{port} && head -c 4 <&3");
        side.agent(&["timeout", "8", "bash", "-c", &open])
    };
    let resolves = |name: &str| side.agent(&["getent", "hosts", name]).status.success();
    let until = |what: &str, done: &dyn Fn() -> bool| {
        let started = Instant::now();
        while !done() {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "waited 60 s for {what}"
            );
            std::thread::sleep(Duration::from_millis(500));
        }
    };
    assert!(
        !greeting("github.com", 22).status.success(),
        "open before it was listed"
    );
    let set = side.root(&["zephyr", "set", "network.allow", "github.com:22"]);
    assert!(set.status.success(), "{}", text(&set));
    until("github.com to resolve for the agent", &|| {
        resolves("github.com")
    });
    let ssh = greeting("github.com", 22);
    assert_eq!(text(&ssh), "SSH-", "the listed host and port");
    // Only that port of it, only that name, and nothing as the resolver the daemon asks.
    let started = Instant::now();
    assert!(
        !greeting("github.com", 443).status.success(),
        "an unlisted port opened"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "it was not refused at once"
    );
    assert!(!resolves("gitlab.com"));
    const AS_DNS: &str = "import socket; s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); \
        s.bind(('127.0.0.11', 0)); s.sendto(b'x', ('127.0.0.1', 9))";
    let spoof = side.agent(&["python3", "-c", AS_DNS]);
    assert!(
        !spoof.status.success(),
        "the agent sent a packet as Docker's DNS"
    );
    // An entry that goes takes its route and its name with it.
    let set = side.root(&["zephyr", "set", "network.allow", "example.org:2222"]);
    assert!(set.status.success(), "{}", text(&set));
    until("github.com to stop resolving", &|| !resolves("github.com"));
    let hosts = text(&side.root(&["cat", "/etc/hosts"]));
    assert!(!hosts.contains("github.com"), "{hosts}");
    let pairs = text(&side.root(&["nft", "list", "set", "inet", "zephyr", "allow"]));
    assert!(
        pairs.contains(". 2222") && !pairs.contains(". 22,") && !pairs.contains(". 22 "),
        "{pairs}"
    );
}
