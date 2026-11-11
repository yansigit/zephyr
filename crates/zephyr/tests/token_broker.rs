//! The token broker against a real container (DESIGN §4.21): what the agent can read holds only
//! stand-ins, the proxy swaps one for the real secret on the host it is bound to and refuses it
//! anywhere else, and the agent's tools trust the broker's CA while its keys stay out of reach.
//!
//! Ignored: it needs Docker, the internet (a public echo host, since the proxy trusts only
//! public roots upstream) and a side container already up, beside any live install, with one
//! connector whose secret is bound to that host:
//!
//! ```sh
//! export ZEPHYR_CONTAINER=zephyr-test-a3 ZEPHYR_IMAGE=zephyr-dev:a3-$(git rev-parse --short HEAD)
//! export ZEPHYR_SRC=$PWD HOME=$(mktemp -d) && mkdir "$HOME/p"
//! zephyr init --non-interactive --no-start --projects "$HOME/p" --provider anthropic --model anthropic/claude-sonnet-5
//! zephyr config set attach.port 18790
//! echo 'A3_SECRET=a3-secret-value-0123456789' >> "$HOME/.zephyr-daemon/secrets.env"
//! echo 'A3_QUOTED=a3"quoted/secret-0123456789' >> "$HOME/.zephyr-daemon/secrets.env"
//! # A remote connector holds a real secret or a stand-in as the owner says; rpi's own MCP client
//! # could not use the stand-in, and the test only needs one bound to the echo host.
//! zephyr mcp add echo --url 'https://httpbin.org/get?key=${A3_SECRET}' --header 'X-Key=${A3_SECRET}' --plain
//! zephyr config set mcp.echo.secrets surrogate
//! # A second secret, with characters JSON escapes, sent in `Authorization`.
//! zephyr mcp add quoted --host httpbin.org -- true '${A3_QUOTED}'
//! zephyr up --build
//! cargo test -p zephyr --test token_broker -- --ignored
//! ```
//!
//! `ZEPHYR_TEST_ECHO` names another httpbin than httpbin.org (`/get` answers with the request's
//! `args` and `headers`, `/basic-auth/USER/PASSWORD` checks a Basic credential), and
//! `ZEPHYR_TEST_SECRET` another value.

use std::process::{Command, Output};

use zephyr::agent::{proxy_env, AGENT_PATH};

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
        cmd.args(["exec", "-u", &ids, "-w", "/zephyr", "-e", "HOME=/zephyr"]);
        cmd.args(["-e", &format!("PATH={AGENT_PATH}")]);
        for (key, value) in proxy_env(Some(image_args.trim().to_string())) {
            cmd.args(["-e", &format!("{key}={value}")]);
        }
        cmd.arg(&self.container).args(args);
        cmd.output().expect("running docker")
    }

    fn sh(&self, script: &str) -> String {
        text(&self.agent(&["sh", "-c", script]))
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
#[ignore = "needs Docker, the internet and a side container (see the top of this file)"]
fn the_agent_holds_stand_ins_and_the_proxy_swaps_them_only_where_bound() {
    // Only ever against a side container named in ZEPHYR_CONTAINER; without one (the release's
    // `--ignored` run), skip, and never touch the live `zephyr`.
    let container = std::env::var("ZEPHYR_CONTAINER").unwrap_or_default();
    if container.is_empty() {
        return eprintln!("skipping: set ZEPHYR_CONTAINER to a side container");
    }
    assert!(
        container != "zephyr",
        "set ZEPHYR_CONTAINER to a side container: this test never runs against the live install"
    );
    let docker = Command::new("docker").arg("version").output();
    if !docker.is_ok_and(|o| o.status.success()) {
        return eprintln!("skipping: Docker does not answer");
    }
    let echo = std::env::var("ZEPHYR_TEST_ECHO").unwrap_or_else(|_| "httpbin.org".into());
    let secret =
        std::env::var("ZEPHYR_TEST_SECRET").unwrap_or_else(|_| "a3-secret-value-0123456789".into());
    let side = Side { container };

    // What the agent can read holds a stand-in for the connector's secret, never the secret.
    let published = side.sh("cat /var/lib/zephyr-public/mcp.json");
    assert!(!published.contains(&secret), "{published}");
    let servers: serde_json::Value = serde_json::from_str(&published).expect("mcp.json");
    let stand_in = servers["mcpServers"]["echo"]["headers"]["X-Key"]
        .as_str()
        .expect("the echo connector's stand-in");
    let quoted = servers["mcpServers"]["quoted"]["args"][0]
        .as_str()
        .expect("the quoted connector's stand-in");
    assert!(stand_in.starts_with("zsur_") && stand_in.len() == 45 && quoted != stand_in);
    assert!(
        stand_in[5..].bytes().all(|b| b.is_ascii_hexdigit()),
        "{stand_in}"
    );
    let environment = side.sh("env");
    assert!(!environment.contains(&secret), "{environment}");
    assert!(environment.contains("NODE_EXTRA_CA_CERTS=/var/lib/zephyr-public/ca.pem"));

    // Its keys are not the agent's to read, nor the trust locations its to write.
    for key in ["/var/lib/zephyr/ca/ca.key", "/var/lib/zephyr/broker.key"] {
        let read = side.agent(&["cat", key]);
        assert!(!read.status.success(), "{key}: {}", text(&read));
        assert!(side.root(&["test", "-s", key]).status.success(), "{key}");
    }
    for dir in [
        "/usr/local/share/ca-certificates",
        "/etc/ssl/certs",
        "/var/lib/zephyr-public",
    ] {
        let planted = side.agent(&["sh", "-c", &format!("echo x > {dir}/planted.crt")]);
        assert!(!planted.status.success(), "{dir}");
    }
    let rewritten = side.agent(&["sh", "-c", "echo x >> /var/lib/zephyr-public/ca.pem"]);
    assert!(!rewritten.status.success());

    // On the bound host the server gets the real secret: it checks a Basic password against
    // its path, and says yes only when the proxy put the secret in for the stand-in.
    let checked = |password: &str| {
        side.sh(&format!(
            "curl -sS -m 60 -o /dev/null -w '%{{http_code}}' -u 'user:{stand_in}' 'https://{echo}/basic-auth/user/{password}'"
        ))
    };
    assert_eq!(checked(&secret).trim(), "200");
    assert_eq!(checked("another-password").trim(), "401");
    // And the agent never sees it, though this server sends back all it was sent, in a header
    // and in the URL: in the reply the secret is the stand-in again. The certificate the
    // client saw is the broker's. Each client finds the CA its own way.
    let url = format!("https://{echo}/get?key={stand_in}");
    let curl = side.sh(&format!("curl -sS -m 60 -v -H 'X-Key: {stand_in}' '{url}'"));
    assert!(curl.contains("zephyr token broker"), "{curl}");
    assert!(
        curl.contains(&format!("\"X-Key\": \"{stand_in}\"")),
        "{curl}"
    );
    assert!(curl.contains(&format!("\"key\": \"{stand_in}\"")), "{curl}");
    assert!(!curl.contains(&secret), "{curl}");
    // `curl -u x:TOKEN` sends the secret inside a Basic credential, which this server sends
    // back: what the agent reads is the credential it wrote, not the one that went out.
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let basic = side.sh(&format!(
        "curl -sS -m 60 -u 'x:{stand_in}' 'https://{echo}/get'"
    ));
    assert!(
        basic.contains(&b64.encode(format!("x:{stand_in}"))),
        "{basic}"
    );
    assert!(
        !basic.contains(&b64.encode(format!("x:{secret}"))),
        "{basic}"
    );
    // A stand-in goes into a credential header only: in any other the request is refused, not
    // swapped and not sent on.
    let stray = side.sh(&format!(
        "curl -sS -m 30 -o /dev/null -D - -A 'probe {stand_in}' 'https://{echo}/get'"
    ));
    assert!(
        stray.contains("403 zephyr: A3_SECRET is not swapped in the header user-agent"),
        "{stray}"
    );
    // A secret the server sends back JSON-escaped (`\"`, and `\/` from some) is turned back
    // too: this one holds a quote and a slash, and goes in `Authorization`.
    let escaped = side.sh(&format!(
        "curl -sS -m 60 -H 'Authorization: Bearer {quoted}' 'https://{echo}/get'"
    ));
    assert!(
        escaped.contains(&format!("\"Authorization\": \"Bearer {quoted}\"")),
        "{escaped}"
    );
    assert!(
        !escaped.contains("quoted/secret") && !escaped.contains("a3\\\"quoted"),
        "{escaped}"
    );
    // A part of a reply is not asked for, whatever the client says.
    let ranged = side.sh(&format!(
        "curl -sS -m 60 -r 0-40 -H 'X-Key: {stand_in}' 'https://{echo}/get'"
    ));
    assert!(
        ranged.contains(stand_in) && !ranged.contains(&secret),
        "{ranged}"
    );
    let node = format!(
        "node -e \"fetch('{url}', {{headers: {{'x-key': '{stand_in}'}}}}).then(r => r.text()).then(console.log)\""
    );
    let node = side.sh(&node);
    assert!(
        node.contains(stand_in) && !node.contains(&secret),
        "node: {node}"
    );
    let python = format!(
        "python3 -c \"import urllib.request as u; print(u.urlopen(u.Request('{url}', headers={{'X-Key': '{stand_in}'}}), timeout=60).read().decode())\""
    );
    let python = side.sh(&python);
    assert!(
        python.contains(stand_in) && !python.contains(&secret),
        "python: {python}"
    );
    // git has no repository there; what matters is that it got an HTTP answer, not a
    // certificate error.
    let git = side.sh(&format!(
        "GIT_TERMINAL_PROMPT=0 git ls-remote https://{echo}/get 2>&1"
    ));
    assert!(
        !git.to_lowercase().contains("certificate") && !git.contains("SSL"),
        "git: {git}"
    );
    assert!(
        git.contains("not found") || git.contains("not valid"),
        "git: {git}"
    );
    // The agent's Chromium, through its NSS store.
    let opened = side.sh(&format!("agent-browser open '{url}' 2>&1"));
    let page = side.sh("agent-browser get text body 2>&1");
    let _ = side.sh("agent-browser close 2>&1");
    assert!(
        page.contains(stand_in) && !page.contains(&secret),
        "chromium: {opened}\n{page}"
    );

    // Anywhere else the stand-in is refused, by name, and audited; nothing is sent.
    let refused = side.sh(&format!(
        "curl -sS -m 30 -o /dev/null -w '%{{http_code}}' -H 'X-Key: {stand_in}' http://example.com/"
    ));
    assert_eq!(refused.trim(), "403", "{refused}");
    let said = side.sh(&format!(
        "curl -sS -m 30 -v -o /dev/null -H 'X-Key: {stand_in}' http://example.com/ 2>&1"
    ));
    assert!(
        said.contains("zephyr: the vault token A3_SECRET is not sent to example.com:80"),
        "{said}"
    );
    let rows = text(&side.root(&[
        "sqlite3",
        "/var/lib/zephyr/state.db",
        "SELECT tool || '|' || args || '|' || outcome FROM audit WHERE session = 'net' AND tool = 'swap'",
    ]));
    assert!(
        rows.contains(&format!("swap|A3_SECRET -> {echo}:443|allowed")),
        "{rows}"
    );
    assert!(
        rows.contains(
            "swap|A3_SECRET -> example.com:80|refused: the vault token A3_SECRET is not sent to this host"
        ),
        "{rows}"
    );
    assert!(!rows.contains(&secret) && !rows.contains("zsur_"), "{rows}");

    // A host nothing is bound to stays a plain tunnel: its own certificate, and what it is
    // sent is the stand-in as it is.
    let plain = side.sh("curl -sS -m 60 -v -o /dev/null https://example.com/ 2>&1");
    assert!(plain.contains("issuer:"), "{plain}");
    assert!(!plain.contains("zephyr token broker"), "{plain}");

    // doctor passes its broker checks and lists the decrypted host.
    let doctor = text(&side.root(&["/usr/local/bin/zephyr", "doctor"]));
    for line in [
        "the token broker's CA is the daemon's alone",
        "the CA is in the system bundle",
        "the agent cannot read /var/lib/zephyr/ca/ca.key",
        "the agent cannot add trust in /etc/ssl/certs",
        "mcp.json holds no brokered secret",
    ] {
        let found = doctor.lines().find(|l| l.contains(line));
        assert!(
            found.is_some_and(|l| l.starts_with("ok ")),
            "{line}:\n{doctor}"
        );
    }
    let decrypts = doctor.lines().find(|l| l.contains("zephyr decrypts"));
    assert!(decrypts.is_some_and(|l| l.contains(&echo)), "{doctor}");
    assert!(
        !doctor.contains(&secret) && !doctor.contains("zsur_"),
        "{doctor}"
    );
}
