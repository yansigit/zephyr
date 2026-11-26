# Contributing to Zephyr

Thank you for your interest in contributing to Zephyr!

Before making significant changes, read [DESIGN.md](DESIGN.md) (the architecture and behaviour spec) and [README.md](README.md) (the user-facing reference).

## Prerequisites

- **Rust toolchain:** Workspace minimum `rust-version = "1.91"` (2021 edition, declared in [Cargo.toml](Cargo.toml)). CI and container builds currently use Rust `1.95` with `rustfmt` and `clippy`.
- **Docker:** Required to build or run the container image and container-level tests, with BuildKit (`docker buildx`) and permissions to run `docker` without `sudo`.
- **Test and lint tools (matching CI in [.github/ci.sh](.github/ci.sh)):**
  - `python3` (used by `crates/zephyr/tests/fake-rpi.py` and other test fixtures)
  - `tmux` (optional; `tests/tui.rs` skips if `tmux` is not installed)
  - `cargo-nextest` (CI runs with `.config/nextest.toml`)
  - `cargo-deny` (for `advisories` and `bans` checks in [deny.toml](deny.toml))
  - `shellcheck` (for shell scripts under `deploy/` and `.github/`)

## Building

Build the workspace crates (`zephyr`, `zephyr-cursor`, `zephyr-policy`):

```sh
cargo build
```

Install the `zephyr` host/container CLI locally:

```sh
cargo install --locked --path crates/zephyr
```

Build the container image locally and start it:

```sh
zephyr up --build
```

## Running Tests

Run all standard (non-ignored) workspace tests:

```sh
cargo test
```

Run the full suite the way CI's `test` job runs it (each test in its own process):

```sh
cargo nextest run --workspace --profile ci
```

Run a single integration test file or filter by test name:

```sh
cargo test -p zephyr --test router
cargo test -p zephyr --test router some_name
```

### Ignored Tests

Some integration tests require an external `rpi` binary or a Docker side container:

```sh
# Against a pinned rpi binary (set RPI_BIN to the binary path):
ZEPHYR_REAL_RPI=1 RPI_BIN=/path/to/rpi cargo test --workspace -- --ignored

# Individual ignored suites:
cargo test -p zephyr --test real_rpi -- --ignored
cargo test -p zephyr-policy --test rpi_load -- --ignored
cargo test -p zephyr --test attach -- --ignored

# Network floor test (requires Docker and a dedicated test container, never a live one):
ZEPHYR_CONTAINER=zephyr-test cargo test -p zephyr --test net_floor -- --ignored
```

## Linting and Style

CI's `lint` job runs the following checks (see [.github/ci.sh](.github/ci.sh)):

```sh
shellcheck deploy/install.sh deploy/runner/start.sh deploy/release.sh deploy/bump-rpi.sh .github/ci.sh
cargo fmt --all --check
cargo deny check advisories bans
cargo clippy --all-targets -- -D warnings
```

Expectations:

- Format Rust code with `cargo fmt` and keep `cargo clippy --all-targets -- -D warnings` clean.
- Avoid wildcard dependency versions ([deny.toml](deny.toml) sets `wildcards = "deny"`, with path dependencies allowed).
- Never log secrets, tokens, or private facts, and never include them in `Debug` output, audit rows, or notices.
- Root-owned processes inside the container must never execute binaries from agent-writable paths.
- Update [DESIGN.md](DESIGN.md) and [README.md](README.md) when behaviour or user-facing commands/settings change.

## Opening a Pull Request

1. Keep changes focused on a single bug fix or feature.
2. Use clear, descriptive commit messages (conventional prefix with scope, e.g. `fix(model): ...`).
3. Run `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo deny check advisories bans`, and `cargo test` (or `cargo nextest run --workspace --profile ci`) locally before opening the PR.
4. In the PR description, explain why the change is needed, summarize what changed, and list the checks you ran.
5. Please do not submit security vulnerability reports as public pull requests or issues; see [SECURITY.md](SECURITY.md).
