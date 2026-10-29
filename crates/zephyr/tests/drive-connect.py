#!/usr/bin/env python3
"""Drives stock `rpi --connect` in a pty: wait for the header, type a prompt, wait for
<expect> on screen, then Ctrl+C (exits when idle, sending stop_session).
usage: drive-connect.py <rpi> <addr> <token> <prompt> <expect>"""
import os, pty, sys, time, select, struct, fcntl, termios
rpi, addr, token, prompt, expect = sys.argv[1:6]
pid, fd = pty.fork()
if pid == 0:
    os.execv(rpi, [rpi, "--connect", addr, "--token", token])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
screen = b""
def pump(secs, until=None):
    global screen
    end = time.time() + secs
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.1)
        if r:
            try: screen += os.read(fd, 65536)
            except OSError: return False
        if until and until.encode() in screen: return True
    return False
pump(2, "ready")
for ch in prompt: os.write(fd, ch.encode()); time.sleep(0.01)
os.write(fd, b"\r")
ok = pump(15, expect)
time.sleep(0.5); os.write(fd, b"\x03")  # Ctrl+C while idle exits
pump(3)
_, status = os.waitpid(pid, 0)
if not ok: sys.stderr.write(screen.decode(errors="replace"))
print("seen" if ok else "NOT seen", repr(expect), "exit", os.waitstatus_to_exitcode(status))
sys.exit(0 if ok else 1)
