import { invoke } from "@tauri-apps/api/core";
import { useAppStore } from "../store/appStore";

// The invoke bridge only exists inside the Tauri app shell; the web build has
// no plugin layer (same test FileViewer inlines for downloads).
export function inTauriShell(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

// A loopback hub runs on THIS machine, so bridge-reported file paths name
// real local files and native open/reveal is meaningful (the desktop app
// next to a local hub). A LAN hub or a phone must keep the streamed viewer.
export function isLoopbackHub(hubUrl: string): boolean {
  try {
    // WHATWG URLs report IPv6 hosts bracketed ([::1]) — strip before compare.
    const host = new URL(hubUrl).hostname.toLowerCase().replace(/^\[|\]$/g, "");
    return host === "localhost" || host === "::1" || host.startsWith("127.");
  } catch {
    return false;
  }
}

// Both conditions at once, read from the live store at call time (no
// subscription: callers run it inside event handlers).
export function canOpenLocally(): boolean {
  const profile = useAppStore.getState().profile;
  return inTauriShell() && !!profile && isLoopbackHub(profile.hubUrl);
}

// file:///a/b%20c.ts -> /a/b c.ts (null for anything but a file URL with a
// real path — a bare "file://" parses to pathname "/", which names no file).
export function fileUrlToPath(href: string): string | null {
  if (!href.startsWith("file://")) return null;
  try {
    const pathname = new URL(href).pathname;
    return pathname && pathname !== "/" ? decodeURIComponent(pathname) : null;
  } catch {
    return null;
  }
}

export async function openLocalPath(path: string, reveal: boolean): Promise<void> {
  await invoke("open_local_path", { path, reveal });
}
