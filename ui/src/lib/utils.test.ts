import { afterEach, describe, expect, it, vi } from "vitest";

import { toDisplayEndpointUrl } from "./utils";

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
});
