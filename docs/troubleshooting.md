---
title: Troubleshooting
---

# Troubleshooting & FAQ

## Common issues

### `error: could not read config 'fog.json'`

fog looks for `fog.json` in the current working directory. Either create the file there or specify the path with `-c`:

```bash
fog -c /path/to/your/config.json
```

### Proxy shows `ERR` log entries with "bind failed"

Another process is already using the configured port. Either:
- Stop the other process
- Change the `port` in your proxy config
- Check with: `lsof -i :<port>`

### Services show "Failed to spawn"

Common causes:
- The working directory `path` doesn't exist (path is relative to the config file's location)
- The `cmd` binary isn't found in `$PATH`
- The shell (`$SHELL`) is misconfigured

### Terminal output is empty or garbled

If the output appears empty or shows raw escape sequences:
- Make sure the service writes to stdout (some tools write to stderr)
- Check that the terminal emulator you're running fog in supports the ANSI features used by the service

### Clipboard copy doesn't work

OSC 52 clipboard requires terminal support:
- **iTerm2**: Enable "Applications in terminal may access clipboard" in Preferences → General → Selection
- **kitty**: Works out of the box
- **tmux**: Requires `set -g set-clipboard on` in `.tmux.conf`
- **Terminal.app**: Does not support OSC 52

If your terminal doesn't support OSC 52, you can still use the terminal's native text selection.

### Scrollback is too short

Increase `max_scrollback` in your config file:

```json
{
  "max_scrollback": 10000
}
```

Higher values use more memory. Default is 2000 lines per terminal.

### Proxy is slow / high CPU

- The proxy uses a single-threaded Tokio runtime. Under high concurrency, this may become a bottleneck.
- Each request is fully buffered (body collected before forwarding) — large uploads will use memory.
- The log buffer is capped at `max_log_entries` (default: 1000). With very high traffic, the UI may have trouble keeping up. Increase the value or reduce request frequency.

### Config changes don't apply to services

Only theme and proxy settings are hot-reloaded. Service entries (add/remove/modify) require restarting fog.

### `cargo install` fails

```bash
error: failed to compile 'fog v0.1.0', intermediate artifacts can be found at...
```

Ensure you have a recent Rust toolchain installed:

```bash
rustup update stable
```

### The app doesn't start on Windows

fog currently has platform-specific code only for macOS and Linux (process tree killing). It may not build or run correctly on Windows.

## FAQ

### Can I run fog as a daemon?

No — fog is designed as a development tool that runs in the foreground. For production deployment, use a dedicated process manager.

### Can I use fog with Docker?

Yes, but the service paths in the config are relative to the config file. When running inside a container, mount your project directory and set the paths accordingly.

### Does fog support HTTP/2?

No. The proxy uses HTTP/1.1 only.

### Can I have multiple proxy instances?

No — fog supports a single proxy instance per session.

### Can I use fog without a config file?

No — at minimum, fog needs a config file to know what services to run.

### Does fog persist logs?

With the `--save-logs` flag, fog writes service output to `temp/<name>.txt` on exit. Otherwise, output is not persisted.

## Getting help

If you encounter a bug or have a feature request, please [open an issue](https://github.com/Naputt1/fog/issues/new/choose) on GitHub.
