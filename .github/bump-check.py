#!/usr/bin/env python3
"""rpi-bump.yml's push job checks the build job's patch with this. The build ran code nobody
reviewed (the new rpi binary, the new crates' build scripts), so the patch is untrusted.

    bump-check.py patch PATCH
        A first gate, before `git apply`: only the files deploy/bump-rpi.sh edits (ALLOWED), each
        an existing text file, with no new, deleted, renamed or binary file and no mode change;
        hunk headers that count their lines; changed lines of printable ASCII and tabs only (no
        carriage return or other control character, nothing non-ASCII).

    bump-check.py verify OLD NEW SHA256_AMD64 SHA256_ARM64
        After `git apply`, run in that checkout: what git actually wrote is compared with the pin
        move, so no trick in the patch's own semantics matters. Only files in ALLOWED changed, in
        place, with their modes kept, and nothing new is in the tree. Each changed file but
        Cargo.lock is byte for byte its HEAD version with, on the lines that mention rpi, OLD
        replaced by NEW (and OLD's minor by NEW's), and the Dockerfile's two rpi sha256s replaced
        by the release's own (the caller fetches them). Cargo.lock, parsed, keeps every package
        that is not an rpi-* crate exactly as it was, keyed by (name, version, source) with no key
        twice, and every package it gains, and every rpi-* one (at NEW), comes from crates.io with a
        checksum; no crate has more versions in it than in main's lock (a new crate, one).

Exits 1, naming the first problem, when the patch is anything else."""
import os
import re
import subprocess
import sys
import tempfile
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


def gate(data):
    """The byte-level first gate on the patch, split on \\n only, as git splits it."""
    try:
        lines = data.decode("utf-8").split("\n")
    except UnicodeDecodeError:
        fail("the patch is not UTF-8")
    if lines and lines[-1] == "":
        lines.pop()
    seen, i = set(), 0
    while i < len(lines):
        m = re.fullmatch(r"diff --git a/([A-Za-z0-9._/-]+) b/([A-Za-z0-9._/-]+)", lines[i])
        if not m or m[1] != m[2]:
            fail(f"not a file header: {lines[i]!r}")
        path = m[1]
        if path not in ALLOWED:
            fail(f"{path} is not a file bump-rpi.sh edits")
        if path in seen:
            fail(f"{path} is in the patch twice")
        seen.add(path)
        i += 1
        if i < len(lines) and re.fullmatch(r"index [0-9a-f]+\.\.[0-9a-f]+( 100644| 100755)?", lines[i]):
            i += 1
        if lines[i : i + 2] != [f"--- a/{path}", f"+++ b/{path}"]:
            fail(f"{path}: only a change to the file's text may be in the patch")
        i += 2
        while i < len(lines) and not lines[i].startswith("diff --git "):
            m = HUNK.fullmatch(lines[i])
            if not m:
                fail(f"{path}: not a hunk header: {lines[i]!r}")
            old_len, new_len = int(m[2] or 1), int(m[4] or 1)
            i += 1
            olds = news = 0
            while i < len(lines) and (olds < old_len or news < new_len or lines[i].startswith("\\")):
                line = lines[i]
                tag = line[:1]
                if line == "\\ No newline at end of file":
                    pass
                elif tag in " -+":
                    if tag != " " and not all(c == "\t" or " " <= c <= "~" for c in line[1:]):
                        fail(f"{path}: a changed line holds a control or non-ASCII character: {line!r}")
                    olds += tag != "+"
                    news += tag != "-"
                else:
                    fail(f"{path}: not a patch line: {line!r}")
                i += 1
            if (olds, news) != (old_len, new_len):
                fail(f"{path}: a hunk does not hold the lines its header counts")
    if not seen:
        fail("the patch is empty")


def git(*args):
    # No system or user config: nothing but the repository itself shapes what git reports.
    env = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", HOME=tempfile.mkdtemp())
    out = subprocess.run(["git", "-c", "core.quotePath=true", *args], env=env, capture_output=True)
    if out.returncode:
        fail(f"git {args[0]}: {out.stderr.decode(errors='replace').strip()}")
    return out.stdout


def pinned(text, path, old, new, amd64, arm64):
    """What deploy/bump-rpi.sh's seds make of a file, rule for rule (keep the two in step): each
    rule is (pattern, replacement, every match on the line or only the first, as sed's `g`)."""
    minor = new.rsplit(".", 1)[0]
    o = re.escape(old)
    rules = []
    if path == "deploy/Dockerfile":
        rules += [(r"^ARG RPI_VERSION=.*", f"ARG RPI_VERSION={new}", False),
                  (r"^ARG RPI_SHA256_AMD64=.*", f"ARG RPI_SHA256_AMD64={amd64}", False),
                  (r"^ARG RPI_SHA256_ARM64=.*", f"ARG RPI_SHA256_ARM64={arm64}", False)]
    if path == "crates/zephyr/src/main.rs":
        rules.append((r'^const RPI_PIN: &str = "rpi [0-9.]*";', f'const RPI_PIN: &str = "rpi {minor}.";', False))
    if path.startswith("crates/") and path.endswith("/Cargo.toml"):
        rules.append((rf'^(rpi-[a-z-]+ = (\{{ version = )?)"={o}"', rf'\g<1>"={new}"', False))
    if path == "crates/zephyr/tests/fake-rpi.py":
        rules.append((rf'print\("rpi {o}"\)', f'print("rpi {new}")', False))
    if path.startswith(("crates/", "deploy/")) or path == "CLAUDE.md":
        rules.append((rf"rpi-{o}/bin/rpi", f"rpi-{new}/bin/rpi", True))
    if path == "README.md":
        rules.append((rf"the upstream `rpi` {o} release", f"the upstream `rpi` {new} release", False))
    if path == "CLAUDE.md":
        rules += [(r"rpi is pinned to [0-9.]*x", f"rpi is pinned to {minor}.x", False),
                  (rf'"={o}"', f'"={new}"', True)]
    out = []
    for line in text.split("\n"):
        for pattern, repl, every in rules:
            # The replacements hold only checked versions and sha256s, so nothing in them escapes.
            line = re.sub(pattern, repl, line, count=0 if every else 1)
        out.append(line)
    return "\n".join(out)


def packages(text, which):
    try:
        lock = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        fail(f"{which} Cargo.lock does not parse: {e}")
    by = {}
    for p in lock.get("package", []):
        key = (p.get("name"), p.get("version"), p.get("source"))
        if key in by:
            fail(f"{which} Cargo.lock lists {key[0]} {key[1]} from {key[2]} twice")
        by[key] = p
    return {k: v for k, v in lock.items() if k != "package"}, by


def check_lock(before, after, new):
    rest_was, was = packages(before, "the original")
    rest_now, now = packages(after, "the written")
    if rest_was != rest_now:
        fail("Cargo.lock: something besides its packages changed")
    crates_io = lambda p: p.get("source") == REGISTRY and SHA.fullmatch(str(p.get("checksum", "")))
    rpi = lambda key: str(key[0]).startswith("rpi-")
    for key, p in was.items():
        if not rpi(key) and now.get(key) != p:
            fail(f"Cargo.lock: {key[0]} {key[1]} is moved or changed, and only the rpi crates may move; "
                 "this bump waits for a person")
    for key, p in now.items():
        if (rpi(key) or key not in was) and not crates_io(p):
            fail(f"Cargo.lock: {key[0]} {key[1]} does not come from crates.io with a checksum")
        if rpi(key) and key[1] != new:
            fail(f"Cargo.lock: {key[0]} is at {key[1]}, not {new}")
    # No crate gains a second version beside main's: a new one is a new name, once.
    count = lambda lock: {n: sum(k[0] == n for k in lock) for n in {k[0] for k in lock}}
    had, has = count(was), count(now)
    for name, n in has.items():
        if n > max(had.get(name, 0), 1):
            fail(f"Cargo.lock: {name} is there {n} times, main's lock has it {had.get(name, 0)}; "
                 "this bump waits for a person")


def verify(old, new, amd64, arm64):
    if not (VERSION.fullmatch(old) and VERSION.fullmatch(new)):
        fail(f"not versions: {old!r} {new!r}")
    if not (SHA.fullmatch(amd64) and SHA.fullmatch(arm64)):
        fail("not sha256s")
    if git("ls-files", "--others", "-z"):
        fail("the patch left a file that is not in the repository")
    raw = git("diff", "--raw", "-z", "--no-renames", "HEAD").split(b"\0")
    changed = []
    for meta, path in zip(raw[0::2], raw[1::2]):
        m = re.fullmatch(rb":(100644|100755) (100644|100755) [0-9a-f]+ [0-9a-f]+ M", meta)
        path = path.decode("utf-8", errors="replace")
        if not m or m[1] != m[2]:
            fail(f"{path}: not a change to the file's text: {meta.decode(errors='replace')}")
        if path not in ALLOWED:
            fail(f"{path} is not a file bump-rpi.sh edits")
        changed.append(path)
    if "deploy/Dockerfile" not in changed:
        fail("the patch does not move deploy/Dockerfile's RPI_VERSION")
    # Every file bump-rpi.sh edits, changed or not, must now be the pin move of its HEAD version.
    tracked = set(git("ls-files", "-z").decode("utf-8").split("\0"))
    for path in sorted(ALLOWED & tracked):
        before = git("show", f"HEAD:{path}").decode("utf-8")
        with open(path, "rb") as f:
            written = f.read()
        try:
            written = written.decode("utf-8")
        except UnicodeDecodeError:
            fail(f"{path}: what was written is not UTF-8")
        if path == "Cargo.lock":
            check_lock(before, written, new)
        elif written != pinned(before, path, old, new, amd64, arm64):
            fail(f"{path}: what was written is not the move from rpi {old} to {new}")


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "patch":
        with open(sys.argv[2], "rb") as f:
            gate(f.read())
        print("bump-check: the patch passes the first gate")
    elif len(sys.argv) == 6 and sys.argv[1] == "verify":
        verify(*sys.argv[2:])
        print(f"bump-check: what was written only moves rpi {sys.argv[2]} to {sys.argv[3]}")
    else:
        fail("usage: bump-check.py patch PATCH | verify OLD NEW SHA256_AMD64 SHA256_ARM64")
