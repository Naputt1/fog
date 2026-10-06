#!/usr/bin/env bash
#
# Rebuilds the demo project used by assets/kill-progress.tape and records
# assets/kill-progress.gif.
#
# The demo is a self-contained git repo in /tmp/fog-kill-demo with a three
# service fog.json (db/api/web). api and web declare a `shutdown_cmd`, so the
# tape shows `fog kill` rendering each service's shutdown command (or its last
# log lines when it has none) while the instance drains.
#
# Requires: vhs, ffmpeg, python3, and a built target/release/fog.
#
# Usage: scripts/record-kill-progress.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEMO_DIR=/tmp/fog-kill-demo
DEMO_TMP=/tmp/fog-kill-demo-tmp
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

# Stop any instance left behind by an interrupted or previous recording.
if [[ -d "$DEMO_DIR" ]]; then
  (cd "$DEMO_DIR" && TMPDIR="$DEMO_TMP" "$BIN" kill --all --force >/dev/null 2>&1) || true
fi

echo "==> Recreating $DEMO_DIR"
rm -rf "$DEMO_DIR" "$DEMO_TMP"
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
          "health_check": { "kind": "tcp", "target": "127.0.0.1:${ports.db}" }
        },
        {
          "name": "api",
          "path": ".",
          "cmd": "python3 svc.py api ${ports.api}",
          "depends_on": ["db"],
          "shutdown_cmd": "docker compose -f infra/docker-compose.yml down api",
          "health_check": { "kind": "tcp", "target": "127.0.0.1:${ports.api}" }
        },
        {
          "name": "web",
          "path": ".",
          "cmd": "python3 svc.py web ${ports.web}",
          "depends_on": ["api"],
          "shutdown_cmd": "docker compose -f infra/docker-compose.yml down web",
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
)

echo "==> Recording $REPO_ROOT/assets/kill-progress.gif"
(cd "$REPO_ROOT" && vhs assets/kill-progress.tape)

# Still frame for the PR/docs: ~8s in, while db is draining (its last log
# lines) and api/web show their shutdown_cmd.
echo "==> Extracting still frame"
ffmpeg -y -v error -ss 8 -i "$REPO_ROOT/assets/kill-progress.gif" -frames:v 1 \
  -update 1 "$REPO_ROOT/assets/kill-progress.png"

# The tape ends with a torn-down stack; sweep anything left behind.
if [[ -d "$DEMO_DIR" ]]; then
  (cd "$DEMO_DIR" && TMPDIR="$DEMO_TMP" "$BIN" kill --all --force >/dev/null 2>&1) || true
fi

echo "==> Done: $REPO_ROOT/assets/kill-progress.gif ($(du -h "$REPO_ROOT/assets/kill-progress.gif" | cut -f1))"
