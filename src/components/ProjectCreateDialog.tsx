import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  ArrowLeft,
  ArrowUp,
  Folder,
  FolderPlus,
  Loader2,
  Search,
} from "lucide-react";
import { HubApiError, HubClient } from "../lib/hub";
import { useBackHandler } from "../lib/backNav";
import { loadCachedProjects, saveCachedProjects } from "../lib/storage";
import type { FsBrowseResult, HubProject } from "../lib/types";
import { fmtRelative } from "../lib/time";
import { useAppStore } from "../store/appStore";

// Remote session-create (bridge 0.17.0, ADR-0014): pick one of the machine's
// known projects and start a NEW CLI session in it. The hub spawns (or
// reuses) a serve bridge for the project — a visible terminal REPL on the
// desktop since ADR-0016. The list below is the hub's whitelist; the BROWSE
// mode (bridge 0.65.0) walks the machine's directories instead and starts a
// session in any of them the hub flags creatable (a known project, or under
// a configured remote.projectRoots root).

export function projectName(path: string): string {
  const parts = path.split("/").filter(Boolean);
  return parts[parts.length - 1] ?? path;
}

export function ProjectCreateDialog({ onClose }: { onClose: () => void }) {
  const { t, i18n } = useTranslation();
  const profile = useAppStore((s) => s.profile);
  const createProjectSession = useAppStore((s) => s.createProjectSession);
  const [projects, setProjects] = useState<HubProject[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [creating, setCreating] = useState<string | null>(null);
  // Browse mode (bridge 0.65.0): dir is the CURRENT directory's listing;
  // null means loading (dirError set on failure). dirSeq drops stale
  // listings — a slow answer must never repaint a directory already left.
  const [browse, setBrowse] = useState(false);
  const [dir, setDir] = useState<FsBrowseResult | null>(null);
  const [dirError, setDirError] = useState<string | null>(null);
  const dirSeq = useRef(0);

  useBackHandler(() => {
    if (creating) return true; // swallowed — mid-create, no navigation
    if (browse) {
      setBrowse(false);
      setDir(null);
      setDirError(null);
    } else {
      onClose();
    }
    return true;
  });

  // This sheet now also opens from inside a session. A post-connect failure
  // leaves activeSessionId null, so the route to InstancePicker takes over and
  // unmounts us before the action resolves — consuming the notice here would
  // swallow it. When we are already gone, let the destination banner show it.
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  useEffect(() => {
    if (!profile) return;
    // Paint the persisted list at once (null → the loading placeholder);
    // the fetch below refreshes and re-persists it.
    setProjects(loadCachedProjects(profile.hubUrl));
    const client = new HubClient(profile.hubUrl, profile.token);
    let alive = true;
    client
      .projects()
      .then((list) => {
        if (!alive) return;
        setProjects(list);
        saveCachedProjects(profile.hubUrl, list);
      })
      .catch((e) => {
        if (!alive) return;
        // Older bridge (<0.17.0): 404 "not found" — the route doesn't exist.
        const msg =
          e instanceof HubApiError && e.status === 404
            ? t("projectDialog.tooOld")
            : `projects: ${e instanceof Error ? e.message : String(e)}`;
        setError(msg);
      });
    return () => {
      alive = false;
    };
  }, [profile, t]);

  const pick = async (workspacePath: string) => {
    if (creating) return;
    setCreating(workspacePath);
    setError(null);
    setDirError(null);
    // The action reports success itself: this sheet can open from the entry
    // screen (route swap unmounts it) OR from the in-session drawer, where
    // nothing unmounts and a route-swap heuristic would misread the result.
    const ok = await createProjectSession(workspacePath);
    if (ok) {
      onClose();
      return;
    }
    // Unmounted mid-flight: the destination screen owns the notice.
    if (!mounted.current) return;
    // Failure sets `notice` — surface it HERE as well: this dialog's
    // full-screen overlay hides the picker's (or the chat's) notice banner.
    const n = useAppStore.getState().notice;
    if (n) {
      setError(n.startsWith("notice.") ? t(n) : n);
      // Consumed here — don't repeat it on the banner after close.
      useAppStore.getState().dismissNotice();
    }
    setCreating(null);
  };

  // Fetch one directory level. `first` distinguishes the two 404 meanings:
  // entering browse lists HOME, which always exists — a 404 there is a
  // bridge too old for /api/fs/list; later 404s are gone directories.
  const openDir = async (path: string | undefined, first = false) => {
    if (!profile) return;
    const seq = ++dirSeq.current;
    setDir(null);
    setDirError(null);
    try {
      const listing = await new HubClient(
        profile.hubUrl,
        profile.token,
      ).fsBrowse(path);
      if (dirSeq.current !== seq) return;
      setDir(listing);
    } catch (e) {
      if (dirSeq.current !== seq) return;
      const msg =
        first && e instanceof HubApiError && e.status === 404
          ? t("projectDialog.tooOld")
          : `browse: ${e instanceof Error ? e.message : String(e)}`;
      setDirError(msg);
    }
  };

  const visible = (projects ?? []).filter((p) =>
    p.workspacePath.toLowerCase().includes(filter.trim().toLowerCase()),
  );
  // Hidden (`.`-prefixed) entries stay server-visible but off the picker.
  const subdirs = (dir?.entries ?? []).filter((e) => !e.name.startsWith("."));

  return (
    <div className="fixed inset-0 z-50 flex items-end justify-center bg-black/60 p-3 pb-[max(var(--safe-bottom),1rem)]">
      <div className="flex max-h-[75vh] w-full max-w-md flex-col overflow-hidden rounded-2xl border border-hairline bg-surface">
        <div className="flex items-center gap-2 px-4 pt-3">
          {browse ? (
            <button
              onClick={() => {
                if (creating) return;
                setBrowse(false);
                setDir(null);
                setDirError(null);
              }}
              disabled={creating !== null}
              aria-label={t("files.back")}
              className="-ml-1 flex size-6 shrink-0 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active disabled:opacity-50"
            >
              <ArrowLeft className="size-4" />
            </button>
          ) : (
            <FolderPlus className="size-4 shrink-0 text-info" />
          )}
          <h2 className="min-w-0 flex-1 truncate text-sm font-semibold text-ink">
            {browse ? t("projectDialog.browse") : t("projectDialog.title")}
          </h2>
          {!browse && (
            <button
              onClick={() => {
                setBrowse(true);
                void openDir(undefined, true);
              }}
              aria-label={t("projectDialog.browse")}
              className="flex size-6 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active"
            >
              <Folder className="size-4" />
            </button>
          )}
          <button onClick={onClose} className="text-faint" aria-label="close">
            ✕
          </button>
        </div>
        {browse ? (
          <p className="truncate px-4 pt-1 font-mono text-2xs text-faint">
            {creating ? t("projectDialog.creatingHint") : (dir?.path ?? "")}
          </p>
        ) : (
          <p className="px-4 pt-1 text-xs text-faint">
            {creating
              ? t("projectDialog.creatingHint")
              : t("projectDialog.hint")}
          </p>
        )}

        {browse ? (
          <>
            <div className="px-4 py-2">
              <button
                onClick={() => dir && void pick(dir.path)}
                disabled={!dir?.creatable || creating !== null}
                className="flex w-full items-center justify-center gap-2 rounded-xl bg-ink px-4 py-2.5 text-sm font-medium text-canvas active:brightness-90 disabled:opacity-40"
              >
                {creating === dir?.path && (
                  <Loader2 className="size-4 animate-spin" />
                )}
                {t("projectDialog.startHere")}
              </button>
              {dir && !dir.creatable && (
                <p className="pt-1.5 text-2xs leading-snug text-faint">
                  {t("projectDialog.notAllowed")}
                </p>
              )}
            </div>
            <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-2">
              {dirError && (
                <p className="mx-2 rounded-lg bg-warning-wash px-3 py-2 text-xs text-warning ring-1 ring-warning/30">
                  {dirError}
                </p>
              )}
              {!dir && !dirError && (
                <div className="flex items-center justify-center gap-2 px-4 py-8 text-xs text-faint">
                  <Loader2 className="size-4 animate-spin" />
                  {t("projectDialog.loadingDirs")}
                </div>
              )}
              {dir && dir.parent && (
                <button
                  onClick={() => void openDir(dir.parent!)}
                  disabled={creating !== null}
                  className="mb-1 flex w-full items-center gap-2 rounded-xl px-3 py-2.5 text-left text-sm text-dim hover:bg-hover active:bg-active disabled:opacity-50"
                >
                  <ArrowUp className="size-4 shrink-0" />
                  <span className="min-w-0 flex-1 truncate font-mono text-xs">
                    {dir.parent}
                  </span>
                </button>
              )}
              {dir && subdirs.length === 0 && !dirError && (
                <p className="px-4 py-6 text-center text-xs text-faint">
                  {t("projectDialog.emptyDir")}
                </p>
              )}
              {dir &&
                subdirs.map((e) => (
                  <button
                    key={`${dir.path}/${e.name}`}
                    onClick={() => void openDir(`${dir.path}/${e.name}`)}
                    disabled={creating !== null}
                    className="mb-1 flex w-full items-center gap-2 rounded-xl px-3 py-2.5 text-left hover:bg-hover active:bg-active disabled:opacity-50"
                  >
                    <Folder className="size-4 shrink-0 text-info" />
                    <span className="min-w-0 flex-1 truncate text-sm text-ink">
                      {e.name}
                    </span>
                  </button>
                ))}
              {dir?.truncated && dir.entries.length > 0 && (
                <p className="px-4 py-2 text-center text-2xs text-faint">
                  {t("projectDialog.listTruncated")}
                </p>
              )}
            </div>
          </>
        ) : (
          <>
            <div className="flex items-center gap-2 px-4 py-2">
              <Search className="size-3.5 shrink-0 text-faint" />
              <input
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
                placeholder={t("projectDialog.filter")}
                className="w-full bg-transparent text-sm text-ink outline-none placeholder:text-faint"
              />
            </div>

            <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-2">
              {error && (
                <p className="mx-2 rounded-lg bg-warning-wash px-3 py-2 text-xs text-warning ring-1 ring-warning/30">
                  {error}
                </p>
              )}
              {!projects && !error && (
                <div className="flex items-center justify-center gap-2 px-4 py-8 text-xs text-faint">
                  <Loader2 className="size-4 animate-spin" />
                  {t("projectDialog.loading")}
                </div>
              )}
              {projects && visible.length === 0 && !error && (
                <p className="px-4 py-6 text-center text-xs text-faint">
                  {t("projectDialog.empty")}
                </p>
              )}
              {visible.map((p) => (
                <button
                  key={p.workspacePath}
                  onClick={() => void pick(p.workspacePath)}
                  disabled={creating !== null}
                  className="mb-1 flex w-full flex-col items-start gap-0.5 rounded-xl px-3 py-2.5 text-left hover:bg-hover active:bg-active disabled:opacity-50"
                >
                  <span className="flex w-full items-center gap-2">
                    <span className="min-w-0 flex-1 truncate text-sm text-ink">
                      {projectName(p.workspacePath)}
                    </span>
                    {creating === p.workspacePath ? (
                      <Loader2 className="size-4 shrink-0 animate-spin text-info" />
                    ) : (
                      <span className="shrink-0 text-2xs text-faint">
                        {t("projectDialog.sessionCount", { count: p.sessions })}
                      </span>
                    )}
                  </span>
                  <span className="flex w-full items-center gap-2 text-2xs text-faint">
                    <span className="min-w-0 flex-1 truncate font-mono">
                      {p.workspacePath}
                    </span>
                    <span className="shrink-0">
                      {fmtRelative(p.lastActive, i18n.language)}
                    </span>
                  </span>
                </button>
              ))}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
