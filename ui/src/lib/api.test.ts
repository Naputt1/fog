import { afterEach, describe, expect, it, vi } from "vitest";

import { parseLogLine, subscribeLogs } from "./api";

/** Minimal EventSource stand-in that captures subscribers and lets tests emit. */
class FakeEventSource {
  static instances: FakeEventSource[] = [];
  readonly url: string;
  closed = false;
  private listeners = new Map<string, Array<(ev: MessageEvent) => void>>();

  constructor(url: string) {
    this.url = url;
    FakeEventSource.instances.push(this);
  }

  addEventListener(type: string, cb: (ev: MessageEvent) => void): void {
    const list = this.listeners.get(type) ?? [];
    list.push(cb);
    this.listeners.set(type, list);
  }

  close(): void {
    this.closed = true;
  }

  emit(type: string, data: string): void {
    for (const cb of this.listeners.get(type) ?? []) {
      cb({ data } as MessageEvent);
    }
  }
}

/** A Go slog JSON line — the exact shape air forwards from the api service. */
const SLOG_LINE =
  '{"time":"2026-09-14T11:07:10.703564+07:00","level":"INFO","msg":"otp code generated (development sender)","channel":"sms","target":"+66810000001","dev_otp_code":"787022"}';

afterEach(() => {
  FakeEventSource.instances = [];
  vi.unstubAllGlobals();
});

describe("parseLogLine", () => {
  it("keeps a JSON log line (slog) as raw text", () => {
    expect(parseLogLine(SLOG_LINE)).toEqual({ text: SLOG_LINE });
  });

  it("keeps plain text unchanged", () => {
    expect(parseLogLine("air: building...")).toEqual({
      text: "air: building...",
    });
  });

  it("keeps malformed brace-prefixed text raw", () => {
    expect(parseLogLine("{not json")).toEqual({ text: "{not json" });
  });

  it("unwraps a structured envelope that has a text field", () => {
    expect(parseLogLine('{"text":"hello","level":"info"}')).toEqual({
      text: "hello",
      level: "info",
    });
  });

  it("does not unwrap a JSON object without a text field", () => {
    expect(parseLogLine('{"msg":"hi"}')).toEqual({ text: '{"msg":"hi"}' });
  });
});

describe("subscribeLogs", () => {
  it("forwards slog JSON lines instead of dropping them", () => {
    vi.stubGlobal("EventSource", FakeEventSource);
    const seen: string[] = [];
    const unsubscribe = subscribeLogs("api", {
      pid: 42,
      onLine: (line) => seen.push(line.text),
    });

    const es = FakeEventSource.instances[0];
    expect(es.url).toBe("/logs/stream?service=api&pid=42");
    es.emit("message", SLOG_LINE);
    es.emit("message", "plain line");
    expect(seen).toEqual([SLOG_LINE, "plain line"]);

    unsubscribe();
    expect(es.closed).toBe(true);
  });
});
