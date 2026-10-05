---
title: Themes
---

# Themes

fog supports color customization through the `theme` field in `fog.json`.

## Theme fields

| Field | Default | Applies to |
|-------|---------|------------|
| `bg` | `#0d0d0f` | App background |
| `surface` | `#161619` | Header, sidebar rail, status bar |
| `surface_alt` | `#1e1e22` | Selected/active row fill |
| `border` | `#2a2a2f` | Separators and popup borders |
| `text` | `#e6e6e7` | Primary text |
| `text_muted` | `#8f8f98` | Dimmed metadata and section labels |
| `accent` | `#7aa2f7` | Brand, selection bar, focus |
| `proxy` | `#7dcfff` | Proxy tab name, WebSocket indicator, filter prompt |
| `terminal` | `#9ece6a` | Shell terminal tab names in sidebar |
| `stopped` | `#f7768e` | Stopped service indicator dot and name |
| `highlight` | `#7aa2f7` | Selected tab highlight |
| `status_200` | `#9ece6a` | HTTP 2xx status codes in proxy log |
| `status_300` | `#e0af68` | HTTP 3xx status codes in proxy log |
| `status_400` | `#f7768e` | HTTP 4xx status codes in proxy log |
| `status_500` | `#f7768e` | HTTP 5xx status codes in proxy log |
| `scrollbar` | `#3b3b44` | Scrollbar thumb and track |
| `selection_bg` | `#3b4261` | Selected row fill (sidebar, menus) |
| `selection_fg` | `#e6e6e7` | Text drawn on the selected row |
| `title` | `#7aa2f7` | Panel titles embedded in borders |
| `key` | `#e0af68` | Keybar keycaps |
| `tab_colors` | `#7dcfff`, `#bb9af7`, `#7aa2f7`, `#9ece6a`, `#e0af68`, `#f7768e` | Per-tab accent hues, assigned by tab order and cycled |

`tab_colors` is an array of colors, not a single value. Each tab's panel border
and title take the next hue in order (proxy, db, api, web, shells, ...) and wrap
around when there are more tabs than colors.

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

### Neutral (the default)

```json
{
  "theme": {
    "bg": "#0d0d0f",
    "surface": "#161619",
    "surface_alt": "#1e1e22",
    "border": "#2a2a2f",
    "text": "#e6e6e7",
    "text_muted": "#8f8f98",
    "accent": "#7aa2f7"
  }
}
```

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
