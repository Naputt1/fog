# Terminal WebSocket Protocol

`fog` exposes a built-in WebSocket endpoint that bridges a browser terminal
(such as [xterm.js](https://xtermjs.org/)) to a live interactive shell running
in a local PTY. This document specifies the wire protocol between the browser
client and `fog`.

## Endpoint

```
GET /ws/terminal
GET /ws/terminal?service=<name>[&auth_token=...]
```

The request must be a standard WebSocket upgrade:

- Method: `GET`
- Path: `/ws/terminal`
- `Upgrade: websocket`
- `Connection: Upgrade`

The endpoint is served **before** route matching, so it is always available
regardless of configured routes or host rules.

### Authentication (optional)

When `terminal.auth_token` is set in the script config, every upgrade request
must pass the shared secret as an `auth_token` query parameter:

```
GET /ws/terminal?auth_token=s3cret
```

A request without the parameter, or with a mismatched value, is rejected with
`401 Unauthorized` before any socket or PTY is created.

### Handshake

On a valid upgrade request `fog` responds with:

```
HTTP/1.1 101 Switching Protocols
Connection: Upgrade
Upgrade: websocket
Sec-WebSocket-Accept: <computed from Sec-WebSocket-Key>
```

The `Sec-WebSocket-Accept` header is derived from the client's
`Sec-WebSocket-Key` per RFC 6455 §4.2.2, so standard browser `WebSocket`
clients complete the handshake.

Once the handshake completes, a shell is spawned in a fresh PTY. The shell
binary is taken from the `SHELL` environment variable and defaults to `bash`.

### Service attach (optional)

```
GET /ws/terminal?service=api
GET /ws/terminal?service=web&auth_token=s3cret
```

When `service` is supplied, the index server resolves it against running fog
instances (IPC discovery) and spawns the shell **in the service's working
directory** with its declared `env` vars (cwd+env attach). This emulates a
terminal "from the service" - you get a shell that looks like you are inside
that service's workdir, without disturbing the live service process (which
keeps running).

- Unknown or non-running service name → `404` (not a shell).
- Omitted `service` → ephemeral shell in daemon cwd (generic terminal).

**Limitation:** This is a *working-directory* attach, not a live MasterPty share.
True sharing of the service's live PTY would require the web server to run in
the same fog daemon process (or IPC FD passing via `SCM_RIGHTS`), because the
service's `MasterPty` lives in `App.items: Vec<Terminal>` and the standalone
`fog index serve` is a separate process. The current design keeps services
untouched and is reversible; live-share can be added later via IPC handoff.

## Data flow

| Direction   | Frame type | Payload                        | Handling |
| ----------- | ---------- | ------------------------------ | -------- |
| client→server | Binary  | raw bytes                   | written verbatim to the PTY master (keystrokes, ANSI input) |
| client→server | Text    | `{"type":"resize",...}`     | resizes the PTY (not forwarded as input) |
| server→client | Binary  | raw ANSI/terminal output     | forwarded verbatim from the PTY master |
| client→server | Ping    | any                          | answered with a `Pong` |
| server→client | Ping    | empty                        | keep-alive, sent every **30 s** |

### Resize

To update the PTY's window size, send a text frame:

```json
{"type":"resize","cols":120,"rows":40}
```

- `cols` and `rows` are optional; missing or invalid values fall back to the
  current size. The initial PTY size is `80×24`.
- Dimensions are clamped to the range `1..=2000` on the server.
- A text frame that does **not** parse as a resize command is treated as raw
  input and written to the PTY.

### Frame size limit

Inbound frames (text or binary) larger than `terminal.max_message_bytes`
(default `65536`, 64 KiB) are rejected: the server closes the connection with
WebSocket close code `1009` (message too big).

### Keep-alive and idle

- The server sends a `Ping` every **30 seconds** to keep intermediaries and
  NAT bindings from closing the socket.
- The session is torn down after the configured idle timeout (default
  **900 seconds / 15 minutes**) of inactivity. Inactivity is measured from the
  last client input or PTY output; pings do not reset the idle timer.

## Session lifecycle

1. The upgrade handshake completes.
2. A PTY is opened and the login shell is spawned.
3. Raw bytes are streamed bidirectionally until one of:
   - the client closes the WebSocket,
   - the shell exits (PTY reaches EOF),
   - the session idles out (15 minutes), or
   - an inbound frame exceeds the size limit (close code `1009`).
4. On teardown the shell is killed and the PTY is released.

## Limits and backpressure

| Setting | Default | Behavior |
| ------- | ------- | -------- |
| `terminal.max_sessions_per_ip` | `8` | Concurrent sessions per client IP; exceeding returns `429 Too Many Requests` |
| `terminal.max_message_bytes` | `65536` | Per-frame size cap; larger frames close with code `1009` |
| `terminal.idle_timeout_secs` | `900` | Session idle timeout |
| `terminal.auth_token` | unset | Optional shared secret required via `auth_token` query parameter (`401`) |
| — (fixed) | `64` | PTY → client output queue capacity in frames; the oldest buffered frame is dropped when full (backpressure, no unbounded memory) |

## Client example (xterm.js)

```js
// If the terminal is behind auth, include the token from your app config:
const auth = token ? `?auth_token=${encodeURIComponent(token)}` : "";
const ws = new WebSocket(`ws://${location.host}/ws/terminal${auth}`);
const term = new Terminal();

term.onData((data) => ws.send(data));           // keystrokes -> PTY
ws.addEventListener("message", (ev) => {
    ev.data.arrayBuffer().then((buf) => term.write(new Uint8Array(buf)));
});

term.onResize(({ cols, rows }) => {
    ws.send(JSON.stringify({ type: "resize", cols, rows }));
});
```

Because PTY output is streamed as **binary** frames, the client must read the
message as an `ArrayBuffer`/`Blob` rather than text.