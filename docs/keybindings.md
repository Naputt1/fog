---
title: Keybindings
---

# Keybindings

fog has three modes: **Normal**, **Terminal Input**, and **Proxy Filter**. The status bar at the bottom shows available actions for the current context.

## Normal mode

Default mode. Navigate tabs, scroll, and access commands.

| Key | Action |
|-----|--------|
| `q` | Quit fog |
| `j` / `→` / `Ctrl+n` | Next tab |
| `k` / `←` / `Ctrl+p` | Previous tab |
| `i` | Enter terminal input mode (not on proxy tab) |
| `R` | Restart current service or proxy |
| `t` / `Ctrl+t` | Open a new shell tab |
| `d` | Close current shell tab (shells only) |
| `s` | Open worktree switch popup (`f` fuzzy search, `Enter` to switch) |
| `↑` | Scroll output up |
| `↓` | Scroll output down |
| `PageUp` | Scroll up by one page |
| `PageDown` | Scroll down by one page |
| `g` / `Home` | Scroll to top |
| `G` / `End` | Scroll to bottom |
| `/` | Enter proxy filter mode (proxy tab only) |
| `?` | Toggle help overlay |
| `Ctrl+q` | Quit fog |

## Terminal Input mode

Entered by pressing `i` on a service or shell tab. Keystrokes are sent directly to the running process.

| Key | Action |
|-----|--------|
| `Esc` | Exit to normal mode |
| Any key | Transmitted to the PTY |

While in input mode, the cursor is shown at the terminal's cursor position (if visible).

## Proxy Filter mode

Entered by pressing `/` on the proxy tab.

| Key | Action |
|-----|--------|
| Any character | Appends to filter query |
| `Backspace` | Removes last character |
| `Enter` | Apply filter and return to normal mode |
| `Esc` | Clear filter and return to normal mode |

The filter is case-insensitive and matches against method, path, status code, and upstream fields.

## Worktree switch popup

Opened by pressing `s` in normal mode on a service tab. Lists every worktree of the current repository. Press `f` to enter fuzzy search: typing filters the list with a case-insensitive subsequence match on the branch label or path.

| Key | Action |
|-----|--------|
| `f` | Enter fuzzy search mode |
| Any character (searching) | Appends to filter (case-insensitive subsequence match on branch or path) |
| `Backspace` (searching) | Removes last filter character |
| `↑` / `↓` | Move selection (wraps around) |
| `Enter` | Switch to the highlighted worktree (exits search) |
| `Esc` | Exit search back to browsing, or close the popup while browsing |
| `d` | Terminate all running instances on the selected branch |

Switching reloads the script's config from the target worktree in place: non-reuse services are torn down, `reuse` services are handed over (their processes keep running), and tabs/proxy are rebuilt. The current worktree is marked with `*`; a green `*` marks a branch that has running instances.

## Mouse

| Action | Effect |
|--------|--------|
| Click on sidebar tab | Switch to that tab |
| Drag-select in content area | Select text (copied to clipboard on release) |
| Scroll wheel up | Scroll output up |
| Scroll wheel down | Scroll output down |

Text selection copies to the system clipboard via the [OSC 52](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html) escape sequence, supported by:
- iTerm2
- kitty
- tmux
- Terminal.app (with restrictions)
- Most xterm-compatible terminals

## Status bar indicators

The bottom border shows relevant commands for the current context:

- **Service tab**: `Q quit | R restart | I input | T new-term`
- **Shell tab**: `Q quit | T new-term | I input | D close`
- **Proxy tab**: `Q quit | R restart | / filter`
- **Input mode**: `Ctrl+Q quit | Esc scroll`

## Help overlay

Press `?` to toggle a centered help overlay showing all keybindings. Press `?` again or any key (except `q`) to dismiss.
