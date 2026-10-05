import { afterEach, describe, expect, it, vi } from "vitest";

import {
  isSafeImageUrl,
  toDisplayEndpointUrl,
  toDisplayUrl,
} from "./utils";

/** Stub the browser host the endpoint URL logic reads from. */
function setHost(hostname: string): void {
  vi.stubGlobal("window", { location: { hostname } });
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("toDisplayEndpointUrl", () => {
  it("keeps the DNS url on localhost", () => {
    setHost("localhost");
    expect(toDisplayEndpointUrl("https://frontend.main.acme/", "53123")).toBe(
      "https://frontend.main.acme/"
    );
  });

  it("rewrites to the current host with the published port on a remote host", () => {
    setHost("192.168.1.10");
    expect(toDisplayEndpointUrl("https://frontend.main.acme/", "53123")).toBe(
      "http://192.168.1.10:53123/"
    );
  });

  it("keeps the path when rewriting", () => {
    setHost("192.168.1.10");
    expect(toDisplayEndpointUrl("https://api.main.acme/v1/", "9000")).toBe(
      "http://192.168.1.10:9000/v1/"
    );
  });

  it("links a port-only endpoint to the current host", () => {
    setHost("192.168.1.10");
    expect(toDisplayEndpointUrl("", "9000")).toBe("http://192.168.1.10:9000");
  });

  it("links a port-only endpoint on localhost too", () => {
    setHost("localhost");
    expect(toDisplayEndpointUrl("", "9000")).toBe("http://localhost:9000");
  });

  it("extracts the host port from a docker mapping", () => {
    setHost("192.168.1.10");
    expect(
      toDisplayEndpointUrl("https://web.main.acme/", "0.0.0.0:53123->53123/tcp")
    ).toBe("http://192.168.1.10:53123/");
  });

  it("returns the url unchanged when no port is published", () => {
    setHost("192.168.1.10");
    expect(toDisplayEndpointUrl("https://web.main.acme/", "")).toBe(
      "https://web.main.acme/"
    );
  });

  it("keeps an endpoint already on the request host", () => {
    setHost("192.168.1.10");
    expect(toDisplayEndpointUrl("http://192.168.1.10:53123/", "53123")).toBe(
      "http://192.168.1.10:53123/"
    );
  });

  it("rejects unsafe URL schemes", () => {
    setHost("192.168.1.10");
    expect(toDisplayEndpointUrl("javascript:alert(1)", "9000")).toBe("");
    expect(toDisplayEndpointUrl("data:text/html,<script/>", "9000")).toBe("");
    expect(toDisplayEndpointUrl("file:///etc/passwd", "9000")).toBe("");
  });

  it("still passes through https", () => {
    setHost("192.168.1.10");
    expect(toDisplayEndpointUrl("https://web.main.acme/", "")).toBe(
      "https://web.main.acme/"
    );
  });
});

describe("toDisplayUrl", () => {
  it("passes https through", () => {
    setHost("192.168.1.10");
    expect(toDisplayUrl("https://web.main.acme/")).toBe(
      "https://web.main.acme/"
    );
  });

  it("rejects unsafe URL schemes", () => {
    setHost("192.168.1.10");
    expect(toDisplayUrl("javascript:alert(1)")).toBe("");
    expect(toDisplayUrl("data:text/html,<script/>")).toBe("");
    expect(toDisplayUrl("file:///etc/passwd")).toBe("");
  });

  it("leaves an empty url unchanged", () => {
    expect(toDisplayUrl("")).toBe("");
  });
});

describe("isSafeImageUrl", () => {
  it("allows http(s)", () => {
    expect(isSafeImageUrl("https://cdn.acme/logo.png")).toBe(true);
    expect(isSafeImageUrl("http://cdn.acme/logo.png")).toBe(true);
  });

  it("allows image data URIs only", () => {
    expect(isSafeImageUrl("data:image/png;base64,AAAA")).toBe(true);
    expect(isSafeImageUrl("data:text/html,<script/>")).toBe(false);
  });

  it("rejects javascript: and other schemes", () => {
    expect(isSafeImageUrl("javascript:alert(1)")).toBe(false);
    expect(isSafeImageUrl("file:///etc/passwd")).toBe(false);
  });
});
