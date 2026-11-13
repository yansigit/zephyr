#!/usr/bin/env python3
"""rpi-bump.yml's push job runs this on the patch the build job made, before it applies it, from the
root of its own checkout:
    .github/bump-check.py OLD NEW SHA256_AMD64 SHA256_ARM64 PATCH
The build job ran code nobody reviewed (the new rpi binary, the new crates' build scripts), so its
patch is untrusted. Passed, the patch only moves the rpi pins from OLD to NEW:
- it touches only the files deploy/bump-rpi.sh edits (ALLOWED), each an existing text file, with
  no new, deleted, renamed or binary file, and its hunk headers count its lines;
- an added or removed line holds printable ASCII and tabs only: no carriage return, which would
  read as a line break here but stays inside the line for `git apply`, nor any other control or
  non-ASCII character;
- in each hunk outside Cargo.lock the added lines are the removed lines, in order, but for the rpi
  version (and its minor) on a line about rpi and the Dockerfile's two rpi sha256s, which must be
  NEW, SHA256_AMD64 and SHA256_ARM64 (the caller fetches those from the release itself);
- Cargo.lock, with the patch applied to this checkout's, keeps every package that is not an rpi-*
  crate exactly as it was, and any package it gains or moves comes from crates.io with a checksum.
Exits 1, naming the first problem, otherwise."""
import re
import sys
import tomllib

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
VERSION = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+")
SHA = re.compile(r"[0-9a-f]{64}")
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
HUNK = re.compile(r"@@ -([0-9]+)(?:,([0-9]+))? \+([0-9]+)(?:,([0-9]+))? @@.*")


def fail(why):
    print(f"bump-check: {why}", file=sys.stderr)
    sys.exit(1)


def parse(data):
    """The patch's files as {path: [(old_start, old_len, [(tag, text)])]}, split on \\n only."""
    try:
        lines = data.decode("utf-8").split("\n")
    except UnicodeDecodeError:
        fail("the patch is not UTF-8")
    if lines and lines[-1] == "":
        lines.pop()
    files, i = {}, 0
    while i < len(lines):
        m = re.fullmatch(r"diff --git a/([A-Za-z0-9._/-]+) b/([A-Za-z0-9._/-]+)", lines[i])
        if not m or m[1] != m[2]:
            fail(f"not a file header: {lines[i]!r}")
        path = m[1]
        if path not in ALLOWED:
            fail(f"{path} is not a file bump-rpi.sh edits")
        if path in files:
            fail(f"{path} is in the patch twice")
        i += 1
        if i < len(lines) and re.fullmatch(r"index [0-9a-f]+\.\.[0-9a-f]+( 100644| 100755)?", lines[i]):
            i += 1
        if lines[i : i + 2] != [f"--- a/{path}", f"+++ b/{path}"]:
            fail(f"{path}: only a change to the file's text may be in the patch")
        i += 2
        hunks = files[path] = []
        while i < len(lines) and not lines[i].startswith("diff --git "):
            m = HUNK.fullmatch(lines[i])
            if not m:
                fail(f"{path}: not a hunk header: {lines[i]!r}")
            old_start, new_start = int(m[1]), int(m[3])
            old_len, new_len = int(m[2] or 1), int(m[4] or 1)
            i += 1
            body, olds, news = [], 0, 0
            while i < len(lines) and (olds < old_len or news < new_len or lines[i].startswith("\\")):
                line = lines[i]
                tag, text = line[:1], line[1:]
                if line == "\\ No newline at end of file":
                    pass
                elif tag in " -+":
                    if tag != " " and not all(c == "\t" or " " <= c <= "~" for c in text):
                        fail(f"{path}: a changed line holds a control or non-ASCII character: {line!r}")
                    body.append((tag, text))
                    olds += tag != "+"
                    news += tag != "-"
                else:
                    fail(f"{path}: not a patch line: {line!r}")
                i += 1
            if (olds, news) != (old_len, new_len):
                fail(f"{path}: a hunk does not hold the lines its header counts")
            hunks.append((old_start, old_len, body))
    if not files:
        fail("the patch is empty")
    return files


def apply(text, hunks, path):
    """The file with the hunks applied where their headers say, as `git apply` first tries."""
    src, out, at = text.split("\n"), [], 0
    for old_start, old_len, body in hunks:
        start = old_start - 1 if old_len else old_start
        if start < at:
            fail(f"{path}: hunks out of order")
        out += src[at:start]
        at = start
        for tag, line in body:
            if tag != "+":
                if at >= len(src) or src[at] != line:
                    fail(f"{path}: the patch does not apply to this checkout's file")
                at += 1
            if tag != "-":
                out.append(line)
    return "\n".join(out + src[at:])


def check_lock(before, after):
    try:
        before, after = tomllib.loads(before), tomllib.loads(after)
    except tomllib.TOMLDecodeError as e:
        fail(f"Cargo.lock does not parse: {e}")
    rest = lambda lock: {k: v for k, v in lock.items() if k != "package"}
    if rest(before) != rest(after):
        fail("Cargo.lock: something besides its packages changed")
    key = lambda p: (p.get("name"), p.get("version"))
    was = {key(p): p for p in before.get("package", [])}
    now = {key(p): p for p in after.get("package", [])}
    for k, p in was.items():
        if not str(k[0]).startswith("rpi-") and now.get(k) != p:
            fail(f"Cargo.lock: {k[0]} {k[1]} is moved or changed, and only the rpi crates may move; "
                 "this bump waits for a person")
    for k, p in now.items():
        if was.get(k) != p and not (p.get("source") == REGISTRY and SHA.fullmatch(p.get("checksum", ""))):
            fail(f"Cargo.lock: {k[0]} {k[1]} does not come from crates.io with a checksum")


def check(old, new, amd64, arm64, data, root="."):
    if not (VERSION.fullmatch(old) and VERSION.fullmatch(new)):
        fail(f"not versions: {old!r} {new!r}")
    if not (SHA.fullmatch(amd64) and SHA.fullmatch(arm64)):
        fail("not sha256s")
    marks = [(old, "\0V"), (new, "\0V"), (old.rsplit(".", 1)[0] + ".", "\0M"), (new.rsplit(".", 1)[0] + ".", "\0M")]
    files = parse(data)
    if "deploy/Dockerfile" not in files:
        fail("the patch does not move deploy/Dockerfile's RPI_VERSION")
    for path, hunks in files.items():
        if path == "Cargo.lock":
            with open(f"{root}/Cargo.lock", encoding="utf-8") as f:
                lock = f.read()
            check_lock(lock, apply(lock, hunks, path))
            continue

        # What bump-rpi.sh's seds may change: the version (and its minor) on a line about rpi, and
        # the Dockerfile's two rpi sha256s. Everything else stays byte for byte.
        def plain(line):
            if "rpi" in line.lower():
                for value, mark in marks:
                    line = line.replace(value, mark)
            if path == "deploy/Dockerfile":
                line = re.sub(r"^(ARG RPI_SHA256_(AMD64|ARM64)=)[0-9a-f]{64}$", "\\1\0H", line)
            return line

        for _, _, body in hunks:
            removed = [plain(t) for tag, t in body if tag == "-"]
            added = [plain(t) for tag, t in body if tag == "+"]
            if removed != added:
                fail(f"{path}: a change other than the rpi version or its sha256s: {added or removed}")
    want = {f"ARG RPI_VERSION={new}", f"ARG RPI_SHA256_AMD64={amd64}", f"ARG RPI_SHA256_ARM64={arm64}"}
    added = {t for _, _, body in files["deploy/Dockerfile"] for tag, t in body if tag == "+"}
    if added != want:
        fail(f"deploy/Dockerfile: the new pins are not rpi {new} and its release's sha256s: {sorted(added)}")


if __name__ == "__main__":
    if len(sys.argv) != 6:
        fail("usage: bump-check.py OLD NEW SHA256_AMD64 SHA256_ARM64 PATCH")
    with open(sys.argv[5], "rb") as f:
        check(*sys.argv[1:5], f.read())
    print(f"bump-check: the patch only moves rpi {sys.argv[1]} to {sys.argv[2]}")
