---
title: Themes
---

# Themes

fog supports color customization through the `theme` field in `fog.json`.

## Theme fields

| Field | Default | Applies to |
|-------|---------|------------|
| `proxy` | `cyan` | Proxy tab name, WebSocket indicator, filter prompt |
| `terminal` | `green` | Shell terminal tab names in sidebar |
| `stopped` | `red` | Stopped service indicator dot and name |
| `highlight` | `magenta` | Selected tab highlight |
| `status_200` | `green` | HTTP 2xx status codes in proxy log |
| `status_300` | `yellow` | HTTP 3xx status codes in proxy log |
| `status_400` | `red` | HTTP 4xx status codes in proxy log |
| `status_500` | `red` | HTTP 5xx status codes in proxy log |

## Color values

Colors can be specified as:

### Named colors

| Name | Description |
|------|-------------|
| `reset` / `default` | Terminal default |
| `black` | ANSI black |
| `red` | ANSI red |
| `green` | ANSI green |
| `yellow` | ANSI yellow |
| `blue` | ANSI blue |
| `magenta` | ANSI magenta |
| `cyan` | ANSI cyan |
| `white` | ANSI white |
| `gray` / `grey` | ANSI gray |
| `dark_gray` / `dark_grey` | ANSI dark gray |
| `light_red` | ANSI light red |
| `light_green` | ANSI light green |
| `light_yellow` | ANSI light yellow |
| `light_blue` | ANSI light blue |
| `light_magenta` | ANSI light magenta |
| `light_cyan` | ANSI light cyan |

### Hex colors

Use 6-digit hex codes with a `#` prefix:

```
"#ff0000"   → red
"#00ff00"   → green
"#0000ff"   → blue
"#1a1a2e"   → dark navy
"#e94560"   → crimson
```

### Color matching

Names are case-insensitive (`"GREEN"`, `"green"`, `"Green"` all work).

Invalid or unrecognized values default to `reset`.

## Example themes

### Dark theme

```json
{
  "theme": {
    "proxy": "#00bcd4",
    "terminal": "#4caf50",
    "stopped": "#f44336",
    "highlight": "#ff9800",
    "status_200": "#4caf50",
    "status_300": "#ffeb3b",
    "status_400": "#ff9800",
    "status_500": "#f44336"
  }
}
```

### Minimal light theme

```json
{
  "theme": {
    "proxy": "blue",
    "terminal": "green",
    "stopped": "light_red",
    "highlight": "light_magenta",
    "status_200": "green",
    "status_300": "yellow",
    "status_400": "light_red",
    "status_500": "red"
  }
}
```

### Monochrome

```json
{
  "theme": {
    "proxy": "white",
    "terminal": "white",
    "stopped": "dark_gray",
    "highlight": "white",
    "status_200": "white",
    "status_300": "gray",
    "status_400": "dark_gray",
    "status_500": "dark_gray"
  }
}
```

## Hot-reloading

Theme changes in the config file are applied at runtime — no restart needed. Simply edit and save `fog.json`.
