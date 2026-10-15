// @vitest-environment node
// Loopback gating for native open: only a hub on THIS machine (localhost /
// loopback) may hand bridge paths to the OS — a LAN hub's paths would name
// files that do not exist locally.
import { describe, expect, test } from "vitest";
import { fileUrlToPath, inTauriShell, isLoopbackHub } from "../src/lib/localOpen";

describe("isLoopbackHub", () => {
  test("accepts localhost in all its spellings (port-insensitive)", () => {
    expect(isLoopbackHub("http://localhost:8377")).toBe(true);
    expect(isLoopbackHub("http://LOCALHOST")).toBe(true);
    expect(isLoopbackHub("http://127.0.0.1:8377")).toBe(true);
    expect(isLoopbackHub("http://127.1.2.3")).toBe(true);
    expect(isLoopbackHub("http://[::1]:8377")).toBe(true);
  });

  test("rejects LAN hosts and garbage", () => {
    expect(isLoopbackHub("http://192.168.1.10:8377")).toBe(false);
    expect(isLoopbackHub("https://hub.example.com")).toBe(false);
    expect(isLoopbackHub("not a url")).toBe(false);
    expect(isLoopbackHub("")).toBe(false);
  });
});

test("inTauriShell is false outside the app shell (no window in node)", () => {
  expect(inTauriShell()).toBe(false);
});

describe("fileUrlToPath", () => {
  test("decodes file URLs to plain paths", () => {
    expect(fileUrlToPath("file:///Users/w/a%20b.ts")).toBe("/Users/w/a b.ts");
    expect(fileUrlToPath("file:///etc/hosts")).toBe("/etc/hosts");
  });

  test("non-file URLs and empty paths are rejected", () => {
    expect(fileUrlToPath("https://example.com/x")).toBeNull();
    expect(fileUrlToPath("file://")).toBeNull();
  });
});
