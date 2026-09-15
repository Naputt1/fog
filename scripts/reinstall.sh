#!/usr/bin/env bash
#
# Rebuild fog from this checkout and atomically replace the installed binary.
#
# Builds the SPA (ui/), compiles the Rust binary, then swaps it in next to the
# existing `fog`. The final step is a rename within the destination directory,
# so it is safe to run while `fog` is running: new invocations use the new
# binary, the running process keeps its old inode until it restarts.
#
# A running index server (the web dashboard) keeps serving its old embedded SPA
# until it is restarted, because it runs from the inode it was spawned with.
# `--restart` kills the detected index server(s) and respawns them from the
# freshly installed binary, so the new UI goes live.
#
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

DEFAULT_INDEX_PORT=18080

build_ui=1
debug=0
restart=0
dest="${FOG_DEST:-}"

usage() {
  cat <<'EOF'
Rebuild fog and replace the installed binary.

Usage: scripts/reinstall.sh [options]

Options:
  --skip-ui       Skip the frontend build (reuse the existing ui/dist).
  --debug         Build the debug profile instead of release.
  --restart       Kill running index server(s) and respawn them from the new
                  binary so the dashboard serves the new UI.
  --dest PATH     Install to PATH (default: the `fog` on $PATH, else
                  $CARGO_HOME/bin/fog).
  -h, --help      Show this help.

Environment:
  FOG_DEST        Same as --dest.
EOF
}

# Ports of live index servers, discovered via the pidfiles the runtime writes
# to $TMPDIR as `fog-index-<port>.pid`. Prints one port per line.
live_index_ports() {
  local tmp f port pid
  tmp="${TMPDIR:-/tmp}"
  for f in "$tmp"/fog-index-*.pid; do
    [ -e "$f" ] || continue
    port="${f##*/fog-index-}"
    port="${port%.pid}"
    case "$port" in ''|*[!0-9]*) continue ;; esac
    pid="$(tr -dc '0-9' <"$f" 2>/dev/null || true)"
    [ -n "$pid" ] || continue
    if kill -0 "$pid" 2>/dev/null; then
      printf '%s\n' "$port"
    fi
  done | sort -u
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --skip-ui) build_ui=0 ;;
    --debug) debug=1 ;;
    --restart) restart=1 ;;
    --dest) dest="${2:?--dest needs a path}"; shift ;;
    --dest=*) dest="${1#*=}" ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

command -v cargo >/dev/null 2>&1 || {
  echo "error: cargo not found on \$PATH" >&2
  exit 1
}

# Resolve the destination binary: --dest/FOG_DEST > $PATH > cargo home.
if [ -z "$dest" ]; then
  dest="$(command -v fog || true)"
fi
if [ -z "$dest" ]; then
  dest="${CARGO_HOME:-$HOME/.cargo}/bin/fog"
fi
# Follow symlinks (e.g. rustup shims) so we replace the real file, not the link.
while [ -L "$dest" ]; do
  target="$(readlink "$dest")"
  case "$target" in
    /*) dest="$target" ;;
    *) dest="$(dirname "$dest")/$target" ;;
  esac
done

echo "==> repo:        $root"
echo "==> destination: $dest"

if [ "$build_ui" = 1 ]; then
  command -v pnpm >/dev/null 2>&1 || {
    echo "error: pnpm not found on \$PATH (use --skip-ui to skip the frontend)" >&2
    exit 1
  }
  echo "==> building frontend (ui/)"
  # --pm-on-fail=ignore tolerates a corepack/pnpm version mismatch with the
  # `packageManager` pin in ui/package.json.
  pnpm --pm-on-fail=ignore -C "$root/ui" install
  pnpm --pm-on-fail=ignore -C "$root/ui" build
else
  echo "==> skipping frontend build (--skip-ui)"
fi

if [ "$debug" = 1 ]; then
  profile_args=(--profile dev)
  out_dir=debug
else
  profile_args=(--release)
  out_dir=release
fi

echo "==> building rust binary ($out_dir)"
cargo build "${profile_args[@]}" --locked --manifest-path "$root/Cargo.toml"

src="$root/target/$out_dir/fog"
[ -x "$src" ] || {
  echo "error: built binary not found: $src" >&2
  exit 1
}

dest_dir="$(dirname "$dest")"
mkdir -p "$dest_dir"
tmp="$dest_dir/.fog.new.$$"
trap 'rm -f "$tmp"' EXIT
cp "$src" "$tmp"
chmod 755 "$tmp"
mv -f "$tmp" "$dest"
trap - EXIT

echo "==> installed: $("$dest" --version)"

# A running index server serves its old embedded SPA until restarted.
ports="$(live_index_ports)"
if [ -n "$ports" ]; then
  if [ "$restart" = 1 ]; then
    echo "==> restarting index server(s) on port(s): $(echo "$ports" | tr '\n' ' ')"
    # `index kill` stops the default port and every `fog-index-*.pid` server.
    "$dest" index kill || true
    for port in $ports; do
      if [ "$port" = "$DEFAULT_INDEX_PORT" ]; then
        "$dest" index serve
      else
        "$dest" index serve --port "$port"
      fi
    done
  else
    echo
    echo "note: index server(s) still running the previous binary on port(s): $(echo "$ports" | tr '\n' ' ')"
    echo "      the dashboard keeps serving the old UI until restarted; re-run with --restart, or:"
    echo "        \"$dest\" index kill && \"$dest\" index serve"
  fi
fi
