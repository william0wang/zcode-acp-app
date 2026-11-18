import type {
  ConnectionProfile,
  HubProject,
  PromptDraft,
  SavedServer,
} from "./types";

// Thin wrapper over localStorage (ADR: app-private WebView storage is
// acceptable for a sideloaded personal client; swap implementations here).

// Legacy single-server key — only read for the one-time migration.
const PROFILE_KEY = "zcode-acp:profile";
const SERVERS_KEY = "zcode-acp:servers";
const LANG_KEY = "zcode-acp:lang";
const FONT_SIZE_KEY = "zcode-acp:font-size";
const PENDING_KEY = "zcode-acp:pending";
const UPDATE_CHANNEL_KEY = "zcode-acp:update-channel";
const PROJECTS_KEY = "zcode-acp:projects";
// Pinned session ids (session-list pinning, requirements 0.4): local-only
// display order, never sent anywhere.
const PINNED_KEY = "zcode-acp:pinned-sessions";

export type Lang = "en" | "zh-CN";
export type FontSize = "small" | "medium" | "large";

// Which ZCode desktop release stream the update screen follows. The bridge's
// install route re-reads the manifest for the channel it is TOLD, so the check
// and the install must agree or the install is refused.
export type UpdateChannel = "stable" | "preview";

// The persisted multi-server state: every saved Hub URL + token pair and
// which one is active. activeId may be null (nothing connected); when set it
// always points at an existing entry.
export interface ServerBook {
  servers: SavedServer[];
  activeId: string | null;
}

function isValidServer(v: unknown): v is SavedServer {
  if (!v || typeof v !== "object") return false;
  const s = v as Partial<SavedServer>;
  return (
    typeof s.id === "string" &&
    s.id.length > 0 &&
    typeof s.name === "string" &&
    s.name.length > 0 &&
    typeof s.hubUrl === "string" &&
    s.hubUrl.length > 0 &&
    typeof s.token === "string"
  );
}

// Default display name: host[:port] of the hub URL, falling back to the raw
// string when it does not parse.
export function defaultServerName(hubUrl: string): string {
  try {
    return new URL(hubUrl).host || hubUrl;
  } catch {
    return hubUrl;
  }
}

export function newServerId(): string {
  return typeof crypto !== "undefined" && "randomUUID" in crypto
    ? crypto.randomUUID()
    : `srv_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 8)}`;
}

// One-time migration: the pre-multi-server single profile becomes the seed
// entry; the legacy key is removed only after the book has been written.
function migrateLegacyProfile(): ServerBook | null {
  try {
    const raw = localStorage.getItem(PROFILE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Partial<ConnectionProfile>;
    if (
      typeof parsed.hubUrl !== "string" ||
      parsed.hubUrl.length === 0 ||
      typeof parsed.token !== "string"
    ) {
      return null;
    }
    const hubUrl = parsed.hubUrl.replace(/\/+$/, "");
    const server: SavedServer = {
      id: newServerId(),
      name: defaultServerName(hubUrl),
      hubUrl,
      token: parsed.token,
    };
    const book: ServerBook = { servers: [server], activeId: server.id };
    localStorage.setItem(SERVERS_KEY, JSON.stringify(book));
    localStorage.removeItem(PROFILE_KEY);
    return book;
  } catch {
    return null;
  }
}

export function loadServerBook(): ServerBook | null {
  const raw = localStorage.getItem(SERVERS_KEY);
  if (raw == null) return migrateLegacyProfile();
  try {
    const parsed = JSON.parse(raw) as { servers?: unknown; activeId?: unknown };
    const servers = Array.isArray(parsed.servers)
      ? parsed.servers.filter(isValidServer)
      : [];
    if (servers.length === 0) return null;
    const activeId =
      typeof parsed.activeId === "string" &&
      servers.some((s) => s.id === parsed.activeId)
        ? parsed.activeId
        : servers[0].id;
    return { servers, activeId };
  } catch {
    return null;
  }
}

export function saveServerBook(book: ServerBook): void {
  localStorage.setItem(SERVERS_KEY, JSON.stringify(book));
}

export function loadLang(): Lang {
  return localStorage.getItem(LANG_KEY) === "zh-CN" ? "zh-CN" : "en";
}

export function saveLang(lang: Lang): void {
  localStorage.setItem(LANG_KEY, lang);
}

export function loadFontSize(): FontSize {
  const v = localStorage.getItem(FONT_SIZE_KEY);
  return v === "medium" || v === "large" ? v : "small";
}

export function saveFontSize(size: FontSize): void {
  localStorage.setItem(FONT_SIZE_KEY, size);
}

export function loadUpdateChannel(): UpdateChannel {
  return localStorage.getItem(UPDATE_CHANNEL_KEY) === "preview" ? "preview" : "stable";
}

export function saveUpdateChannel(channel: UpdateChannel): void {
  localStorage.setItem(UPDATE_CHANNEL_KEY, channel);
}

// Known-projects list per hub, cached for the project dialogs' first paint:
// small, slow-changing, and shared by the history and create sheets — a cold
// open renders instantly while the fetch refreshes in the background.
function isValidProject(v: unknown): v is HubProject {
  if (!v || typeof v !== "object") return false;
  const p = v as Partial<HubProject>;
  return (
    typeof p.workspacePath === "string" &&
    p.workspacePath.length > 0 &&
    typeof p.sessions === "number" &&
    typeof p.lastActive === "number"
  );
}

export function loadCachedProjects(hubUrl: string): HubProject[] | null {
  try {
    const raw = localStorage.getItem(PROJECTS_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    const list = parsed[hubUrl];
    if (!Array.isArray(list)) return null;
    return list.filter(isValidProject);
  } catch {
    return null;
  }
}

export function saveCachedProjects(
  hubUrl: string,
  projects: HubProject[],
): void {
  try {
    const raw = localStorage.getItem(PROJECTS_KEY);
    const parsed = raw
      ? (JSON.parse(raw) as Record<string, unknown>)
      : {};
    parsed[hubUrl] = projects;
    localStorage.setItem(PROJECTS_KEY, JSON.stringify(parsed));
  } catch {
    // Quota/private-mode failures just lose the cache, never the feature.
  }
}

// Queued (pending) prompt drafts per session id — they must survive session
// switches and app restarts, so they persist alongside the profile. Legacy
// entries (plain strings, pre-attachments) load as text-only drafts.

export type PendingPromptMap = Record<string, PromptDraft[]>;

function isDraft(v: unknown): v is PromptDraft {
  if (!v || typeof v !== "object") return false;
  const d = v as { text?: unknown; images?: unknown };
  if (typeof d.text !== "string" || !Array.isArray(d.images)) return false;
  return d.images.every(
    (img) =>
      img &&
      typeof img === "object" &&
      typeof (img as { data?: unknown }).data === "string" &&
      typeof (img as { mimeType?: unknown }).mimeType === "string",
  );
}

export function loadPending(): PendingPromptMap {
  try {
    const raw = localStorage.getItem(PENDING_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    const out: PendingPromptMap = {};
    for (const [sid, val] of Object.entries(parsed)) {
      if (!Array.isArray(val)) continue;
      const drafts = val
        .map((v): PromptDraft | null =>
          typeof v === "string"
            ? { text: v, images: [] }
            : isDraft(v)
              ? v
              : null,
        )
        .filter((v): v is PromptDraft => v !== null);
      if (drafts.length > 0) out[sid] = drafts;
    }
    return out;
  } catch {
    return {};
  }
}

export function savePending(map: PendingPromptMap): void {
  try {
    if (Object.keys(map).length === 0) localStorage.removeItem(PENDING_KEY);
    else localStorage.setItem(PENDING_KEY, JSON.stringify(map));
  } catch {
    // Image payloads can blow past the localStorage quota (a 5 MB passthrough
    // GIF is ~6.7 MB base64). Degrade to text-only drafts rather than losing
    // the queue — staged images then survive only in memory for this run.
    try {
      const stripped = Object.fromEntries(
        Object.entries(map).map(([sid, drafts]) => [
          sid,
          drafts.map((d) => ({ ...d, images: [] })),
        ]),
      );
      if (Object.keys(stripped).length === 0)
        localStorage.removeItem(PENDING_KEY);
      else localStorage.setItem(PENDING_KEY, JSON.stringify(stripped));
    } catch {
      // Storage unavailable — drafts stay in memory for this run.
    }
  }
}

// Pinned sessions (flat id list, newest-pin-last order irrelevant — the list
// sorts by recency within the pinned group).
export function loadPinnedSessions(): string[] {
  try {
    const raw = localStorage.getItem(PINNED_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw) as unknown;
    return Array.isArray(parsed)
      ? parsed.filter((v): v is string => typeof v === "string")
      : [];
  } catch {
    return [];
  }
}

export function savePinnedSessions(ids: string[]): void {
  try {
    if (ids.length === 0) localStorage.removeItem(PINNED_KEY);
    else localStorage.setItem(PINNED_KEY, JSON.stringify(ids));
  } catch {
    // Storage unavailable — pins stay in memory for this run.
  }
}
