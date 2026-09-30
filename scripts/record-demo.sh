#!/usr/bin/env bash
#
# Rebuilds the demo project used by assets/demo.tape and records assets/demo.gif.
#
# The demo is a self-contained git repo in /tmp/fog-demo with a `main` worktree
# and a linked `feat` worktree, plus a three-service fog.json (db/api/web). Each
# service is a tiny Python stand-in that binds a port and logs in colour, so the
# tape needs no docker and no external project. `db` is share: true and random
# (`ports.db = 0`), which is what the tape shows being borrowed rather than
# duplicated.
#
# Requires: vhs, ffmpeg, python3, and a built target/release/fog.
#
# Usage: scripts/record-demo.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEMO_DIR=/tmp/fog-demo
DEMO_FEAT=/tmp/fog-demo-feat
DEMO_TMP=/tmp/fog-demo-tmp
BIN="$REPO_ROOT/target/release/fog"

for tool in vhs ffmpeg python3; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "error: '$tool' is required but not on PATH" >&2
    exit 1
  }
done

if [[ ! -x "$BIN" ]]; then
  echo "==> Building target/release/fog"
  (cd "$REPO_ROOT" && cargo build --release)
fi

# Stop any instances left behind by an interrupted or previous recording, so the
# demo never shows stale rows in `fog ls`. The recorded shell registers instances
# under DEMO_TMP, so the kill must target the same TMPDIR; otherwise it finds
# nothing and `rm -rf` leaves the services orphaned.
for dir in "$DEMO_DIR" "$DEMO_FEAT"; do
  if [[ -d "$dir" ]]; then
    (cd "$dir" && TMPDIR="$DEMO_TMP" "$BIN" kill --all --force >/dev/null 2>&1) || true
  fi
done
# Safety net for an interrupted run: its IPC socket may be gone, so sweep any fog
# daemon or demo service still rooted in the demo directories.
for pid in $(pgrep -f 'release/fog' 2>/dev/null) $(pgrep -f 'svc\.py' 2>/dev/null); do
  cwd=$(lsof -a -p "$pid" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | head -1)
  case "$cwd" in */fog-demo*) kill "$pid" 2>/dev/null || true ;; esac
done

echo "==> Recreating $DEMO_DIR"
rm -rf "$DEMO_DIR" "$DEMO_FEAT" "$DEMO_TMP"
mkdir -p "$DEMO_DIR" "$DEMO_TMP"

cat >"$DEMO_DIR/svc.py" <<'PY'
#!/usr/bin/env python3
"""Tiny stand-in for a real service: binds a port and logs in colour."""
import socket
import sys
import threading
import time

NAME, PORT = sys.argv[1], int(sys.argv[2])
COLOR = {"db": "\033[35m", "api": "\033[36m", "web": "\033[32m"}.get(NAME, "\033[0m")
RESET = "\033[0m"
LOG = {
    "db": ["connection accepted", "checkpoint complete", "autovacuum: 3 rows", "query 1.2ms"],
    "api": ["GET /api/health 200 1ms", "GET /api/items 200 4ms", "POST /api/items 201 6ms", "cache hit /api/items"],
    "web": ["GET / 200 12ms", "GET /assets/app.js 200 3ms", "HMR update pushed", "GET /favicon.ico 200 1ms"],
}


def listen():
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", PORT))
    s.listen(32)
    while True:
        try:
            conn, _ = s.accept()
        except OSError:
            return
        try:
            conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        finally:
            conn.close()


def log(line):
    stamp = time.strftime("%H:%M:%S")
    print(f"{COLOR}{NAME:>3}{RESET} {stamp} {line}", flush=True)


threading.Thread(target=listen, daemon=True).start()
log(f"listening on 127.0.0.1:{PORT}")
lines = LOG.get(NAME, ["."])
i = 0
while True:
    time.sleep(1.1)
    log(lines[i % len(lines)])
    i += 1
PY

cat >"$DEMO_DIR/fog.json" <<'JSON'
{
  "index": { "enabled": false },
  "ports": { "db": 0, "api": 0, "web": 0 },
  "scripts": {
    "dev": {
      "service": [
        {
          "name": "db",
          "path": ".",
          "cmd": "python3 svc.py db ${ports.db}",
          "share": true,
          "health_check": { "kind": "tcp", "target": "127.0.0.1:${ports.db}" }
        },
        {
          "name": "api",
          "path": ".",
          "cmd": "python3 svc.py api ${ports.api}",
          "depends_on": ["db"],
          "health_check": { "kind": "tcp", "target": "127.0.0.1:${ports.api}" }
        },
        {
          "name": "web",
          "path": ".",
          "cmd": "python3 svc.py web ${ports.web}",
          "depends_on": ["api"],
          "health_check": { "kind": "tcp", "target": "127.0.0.1:${ports.web}" }
        }
      ]
    }
  }
}
JSON

(
  cd "$DEMO_DIR"
  git init -q -b main
  git add svc.py fog.json
  git -c user.email=demo@fog.local -c user.name=fog commit -qm "demo: db + api + web"
  git worktree add -q "$DEMO_FEAT" -b feat
)

echo "==> Recording $REPO_ROOT/assets/demo.gif"
(cd "$REPO_ROOT" && vhs assets/demo.tape)

# The tape ends with a live detached stack; stop it so no daemons linger.
for dir in "$DEMO_DIR" "$DEMO_FEAT"; do
  (cd "$dir" && TMPDIR="$DEMO_TMP" "$BIN" kill --all --force >/dev/null 2>&1) || true
done

echo "==> Done: $REPO_ROOT/assets/demo.gif ($(du -h "$REPO_ROOT/assets/demo.gif" | cut -f1))"
