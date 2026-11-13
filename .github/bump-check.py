#!/usr/bin/env python3
"""rpi-bump.yml's push job runs this on the patch the build job made, before it applies it:
    .github/bump-check.py OLD NEW PATCH
The build job ran code nobody reviewed (the new rpi crates' build scripts), so its patch is
untrusted. It may touch only the files deploy/bump-rpi.sh edits (ALLOWED), and no file may be new,
deleted, renamed or binary. In each hunk the added lines must be the removed lines, in order, but
for the rpi version (and its minor) on a line about rpi and the Dockerfile's two rpi sha256s.
Cargo.lock may only gain crates.io packages. Exits 1, naming the first problem, otherwise."""
import re
import sys

ALLOWED = {
    "deploy/Dockerfile",  # RPI_VERSION and both sha256s
    "crates/zephyr/src/main.rs",  # RPI_PIN
    "crates/zephyr/Cargo.toml",  # the `=` pins
    "crates/zephyr-policy/Cargo.toml",
    "crates/zephyr-cursor/Cargo.toml",
    "Cargo.lock",
    "crates/zephyr/tests/fake-rpi.py",  # its --version
    # The real-rpi test path, rpi-<version>/bin/rpi.
    "crates/zephyr/tests/real_rpi.rs",
    "crates/zephyr/tests/attach.rs",
    "crates/zephyr/tests/host.rs",
    "crates/zephyr-policy/tests/rpi_load.rs",
    "crates/zephyr-cursor/tests/e2e.rs",
    "deploy/release.sh",
    # The prose pins.
    "README.md",
    "CLAUDE.md",
}
VERSION = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
LOCK_LINE = re.compile(
    r'^(\[\[package\]\]|name = "[A-Za-z0-9_-]+"|version = "[0-9A-Za-z.+-]+"'
    r'|source = "registry\+https://github\.com/rust-lang/crates\.io-index"'
    r'|checksum = "[0-9a-f]{64}"|dependencies = \[| "[A-Za-z0-9_ .+()-]+",|\]|)$'
)


def fail(why):
    print(f"bump-check: {why}", file=sys.stderr)
    sys.exit(1)


def check(old, new, text):
    if not (VERSION.match(old) and VERSION.match(new)):
        fail(f"not versions: {old!r} {new!r}")
    marks = [(old, "\0V"), (new, "\0V"), (old.rsplit(".", 1)[0] + ".", "\0M"), (new.rsplit(".", 1)[0] + ".", "\0M")]

    # What bump-rpi.sh's seds may change: the version (and its minor) on a line about rpi, and the
    # Dockerfile's two rpi sha256s. Everything else stays byte for byte.
    def plain(line):
        if "rpi" in line.lower():
            for value, mark in marks:
                line = line.replace(value, mark)
        if path == "deploy/Dockerfile":
            line = re.sub(r"^(ARG RPI_SHA256_(AMD64|ARM64)=)[0-9a-f]{64}$", r"\1\0H", line)
        return line

    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    i, files = 0, 0
    while i < len(lines):
        m = re.match(r"^diff --git a/(\S+) b/(\S+)$", lines[i])
        if not m or m[1] != m[2]:
            fail(f"not a file header: {lines[i]!r}")
        path = m[1]
        if path not in ALLOWED:
            fail(f"{path} is not a file bump-rpi.sh edits")
        i += 1
        if i < len(lines) and re.match(r"^index [0-9a-f]+\.\.[0-9a-f]+( 100644| 100755)?$", lines[i]):
            i += 1
        if lines[i : i + 2] != [f"--- a/{path}", f"+++ b/{path}"]:
            fail(f"{path}: only a change to the file's text may be in the patch")
        i += 2
        files += 1
        removed, added = [], []

        def settle():
            if path == "Cargo.lock":
                bad = [a for a in added if not LOCK_LINE.match(a)]
                if bad:
                    fail(f"Cargo.lock gains a line that is not a crates.io package's: {bad[0]!r}")
            elif list(map(plain, removed)) != list(map(plain, added)):
                fail(f"{path}: a change other than the rpi version or a sha256: {added or removed}")
            removed.clear()
            added.clear()

        while i < len(lines) and not lines[i].startswith("diff --git "):
            line = lines[i]
            if line.startswith("@@ "):
                settle()
            elif line.startswith("-"):
                removed.append(line[1:])
            elif line.startswith("+"):
                added.append(line[1:])
            elif not (line.startswith(" ") or line == "\\ No newline at end of file"):
                fail(f"{path}: not a patch line: {line!r}")
            i += 1
        settle()
    if files == 0:
        fail("the patch is empty")


if __name__ == "__main__":
    if len(sys.argv) != 4:
        fail("usage: bump-check.py OLD NEW PATCH")
    with open(sys.argv[3], encoding="utf-8") as f:
        check(sys.argv[1], sys.argv[2], f.read())
    print(f"bump-check: the patch only moves rpi {sys.argv[1]} to {sys.argv[2]}")
