/**
 * Imperative API a terminal/log component exposes to its page so keyboard
 * buttons and scroll controls can drive the terminal without reaching into
 * xterm internals.
 *
 * `dispatchKey` synthesizes real KeyboardEvents on the xterm textarea so
 * xterm's own keybindings (Ctrl+C, Shift+Tab back-tab, application-cursor
 * arrows, …) apply — the mobile keypad composes modifiers exactly like a
 * physical desktop keyboard would.
 */
export interface TerminalHandle {
  /**
   * Dispatch a synthetic keydown + keyup pair through xterm's key handler.
   * `keyCode` is required: xterm's keymap switches on `keyCode`, which real
   * KeyboardEvents carry but synthetic ones leave at 0 — so we shadow it on
   * the constructed event.
   */
  dispatchKey(init: KeyboardEventInit & { key: string }, keyCode: number): void;
  /** Send raw bytes straight to the PTY (no keymap processing). */
  sendRaw(data: string): void;
  /** Scroll the internal scrollback by `amount` lines (negative = up). */
  scroll(amount: number): void;
  scrollToTop(): void;
  scrollToBottom(): void;
  /**
   * Copy the full scrollback + visible buffer text (trimmed per line).
   * Returns the text so the caller can write it to the clipboard.
   */
  copyText(): string;
  focus(): void;
}
