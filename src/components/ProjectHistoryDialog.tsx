import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ArrowLeft, History, Loader2, Search } from "lucide-react";
import { HubApiError, HubClient } from "../lib/hub";
import { useBackHandler } from "../lib/backNav";
import { loadCachedProjects, saveCachedProjects } from "../lib/storage";
import type {
  HubHistoryCursor,
  HubHistorySession,
  HubProject,
} from "../lib/types";
import { fmtRelative } from "../lib/time";
import { useAppStore } from "../store/appStore";
import { SessionList, useLongPress, type SessionRowItem } from "./SessionList";
import { projectName } from "./ProjectCreateDialog";

// Per-project session history (bridge 0.19.0, ADR-0015): browse a project's
// backend session store — closed conversations included — and resume one
// with a tap. Two surfaces, deliberately not reconciled (ADR-0015 §5):
// discovery stays the live-attention list, this is the browse/resume one,
// so the same conversation may appear in both under different ids.
// The listing is fetched strictly on demand for the chosen project: a cold
// project's first page incubates its serve bridge (~12s), so prefetching
// every project would spawn bridges for all of them.

// First page of an opened project, remembered in memory: re-entering a
// project repaints instantly instead of re-paying the serve-bridge
// incubation. Deliberately NOT persisted — sessions churn far faster than
// the project list, and a stale in-memory page is one back-and-forth old.
interface CachedFirstPage {
  rows: HubHistorySession[];
  instanceId: string;
  cursor: HubHistoryCursor | null;
}
const FIRST_PAGE_CACHE_LIMIT = 16;
const firstPageCache = new Map<string, CachedFirstPage>();

function cacheFirstPage(workspacePath: string, page: CachedFirstPage): void {
  firstPageCache.delete(workspacePath);
  firstPageCache.set(workspacePath, page);
  while (firstPageCache.size > FIRST_PAGE_CACHE_LIMIT) {
    const oldest = firstPageCache.keys().next().value;
    if (oldest === undefined) break;
    firstPageCache.delete(oldest);
  }
}

export function ProjectHistoryDialog({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation();
  const profile = useAppStore((s) => s.profile);
  const resumeProjectSession = useAppStore((s) => s.resumeProjectSession);
  const deleteSession = useAppStore((s) => s.deleteSession);
  const deleteProject = useAppStore((s) => s.deleteProject);

  // Project chooser state.
  const [projects, setProjects] = useState<HubProject[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [selected, setSelected] = useState<string | null>(null);

  // Session list state: accumulated pages + the server's composite cursor.
  const [rows, setRows] = useState<HubHistorySession[] | null>(null);
  const [instanceId, setInstanceId] = useState("");
  const [cursor, setCursor] = useState<HubHistoryCursor | null>(null);
  const [listError, setListError] = useState<string | null>(null);
  const [loadingPage, setLoadingPage] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [resuming, setResuming] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);

  // Project-level delete (server ADR-0031): the chooser row's long-press
  // target + its in-flight guard.
  const [projAction, setProjAction] = useState<HubProject | null>(null);
  const [projDeleting, setProjDeleting] = useState(false);

  // Invalidates in-flight listing responses across view switches (back /
  // another project): a late page must never land in the wrong list — its
  // cursor would corrupt the new project's pagination.
  const listSeq = useRef(0);

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
        setError(`projects: ${e instanceof Error ? e.message : String(e)}`);
      });
    return () => {
      alive = false;
    };
  }, [profile]);

  const describeListError = (e: unknown): string => {
    if (e instanceof HubApiError) {
      // Older bridge (<0.19.0): the route doesn't exist.
      if (e.status === 404) return t("historyDialog.tooOld");
      // The project dropped off the known list — refresh and re-pick.
      if (e.status === 403) return t("notice.projectUnknown");
    }
    return `history: ${e instanceof Error ? e.message : String(e)}`;
  };

  const openProject = async (workspacePath: string) => {
    if (!profile || loadingPage) return;
    const mySeq = ++listSeq.current;
    setSelected(workspacePath);
    setListError(null);
    // A remembered first page paints at once — the fresh fetch only runs on
    // a first open (its ~12s incubation is exactly what the cache skips).
    const hit = firstPageCache.get(workspacePath);
    if (hit) {
      setRows(hit.rows);
      setInstanceId(hit.instanceId);
      setCursor(hit.cursor);
      return;
    }
    setRows(null);
    setCursor(null);
    setLoadingPage(true);
    const client = new HubClient(profile.hubUrl, profile.token);
    try {
      // A cold project blocks ~12s while the hub incubates its serve bridge
      // — no client-side timeout, the spinner owns the wait.
      const page = await client.projectSessions(workspacePath);
      if (listSeq.current !== mySeq) return;
      setRows(page.sessions);
      setInstanceId(page.instance.id);
      setCursor(page.nextCursor);
      cacheFirstPage(workspacePath, {
        rows: page.sessions,
        instanceId: page.instance.id,
        cursor: page.nextCursor,
      });
    } catch (e) {
      if (listSeq.current !== mySeq) return;
      setListError(describeListError(e));
    } finally {
      if (listSeq.current === mySeq) setLoadingPage(false);
    }
  };

  const loadMore = async () => {
    if (!profile || !selected || !cursor || loadingMore) return;
    // NOT incremented: this view owns the sequence until back/openProject.
    const mySeq = listSeq.current;
    setLoadingMore(true);
    setListError(null);
    const client = new HubClient(profile.hubUrl, profile.token);
    try {
      // The cursor rides verbatim — recomputing it from the last row's
      // updatedAt (an ISO string) would drop rows tied across the boundary.
      const page = await client.projectSessions(selected, cursor);
      if (listSeq.current !== mySeq) return;
      setRows((prev) => [...(prev ?? []), ...page.sessions]);
      setCursor(page.nextCursor);
    } catch (e) {
      if (listSeq.current !== mySeq) return;
      setListError(describeListError(e));
    } finally {
      if (listSeq.current === mySeq) setLoadingMore(false);
    }
  };

  const resume = async (sessionId: string) => {
    if (resuming || !selected) return;
    setResuming(sessionId);
    setListError(null);
    // The action reports success itself: this sheet can open from the entry
    // screen (route swap unmounts it) OR from the in-session drawer, where
    // nothing unmounts and a route-swap heuristic would misread the result.
    const ok = await resumeProjectSession(selected, sessionId);
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
      setListError(n.startsWith("notice.") ? t(n) : n);
      // Consumed here — don't repeat it on the banner after close.
      useAppStore.getState().dismissNotice();
    }
    setResuming(null);
  };

  const back = () => {
    // A load-more still in flight belongs to the view we are leaving.
    listSeq.current++;
    setSelected(null);
    setRows(null);
    setCursor(null);
    setListError(null);
  };

  // ADR-0031 delete: tombstones the row from every listing. This dialog
  // owns its rows locally, so success just drops the row from view; a
  // failure surfaces inline like resume's (the overlay hides the banner).
  const removeRow = async (sessionId: string) => {
    if (deleting || !instanceId) return;
    setDeleting(sessionId);
    setListError(null);
    const ok = await deleteSession(instanceId, sessionId);
    setDeleting(null);
    if (ok) {
      setRows((prev) =>
        prev ? prev.filter((r) => r.sessionId !== sessionId) : prev,
      );
      // Keep the remembered first page in lockstep so a re-open does not
      // resurrect the tombstoned row.
      if (selected) {
        const cached = firstPageCache.get(selected);
        if (cached)
          cacheFirstPage(selected, {
            ...cached,
            rows: cached.rows.filter((r) => r.sessionId !== sessionId),
          });
      }
      return;
    }
    if (!mounted.current) return;
    const n = useAppStore.getState().notice;
    if (n) {
      setListError(n.startsWith("notice.") ? t(n) : n);
      // Consumed here — don't repeat it on the banner after close.
      useAppStore.getState().dismissNotice();
    }
  };

  // Project delete (server ADR-0031): tombstones the whole workspace — the
  // chooser owns its list locally, so success just drops the row; a failure
  // surfaces in the chooser's error slot like resume's does in the list's.
  const removeProject = async (workspacePath: string) => {
    if (projDeleting) return;
    setProjDeleting(true);
    setError(null);
    const ok = await deleteProject(workspacePath);
    setProjDeleting(false);
    if (ok) {
      setProjAction(null);
      firstPageCache.delete(workspacePath);
      const next = projects
        ? projects.filter((p) => p.workspacePath !== workspacePath)
        : null;
      setProjects(next);
      if (next && profile) saveCachedProjects(profile.hubUrl, next);
      return;
    }
    if (!mounted.current) return;
    const n = useAppStore.getState().notice;
    if (n) {
      setError(n.startsWith("notice.") ? t(n) : n);
      useAppStore.getState().dismissNotice();
    }
  };

  // The gesture mirrors the header's back button: one level up while a
  // project's session list is open, otherwise close the sheet. Both consume.
  useBackHandler(() => {
    if (projAction) {
      setProjAction(null);
    } else if (selected) {
      if (resuming === null) back();
    } else {
      onClose();
    }
    return true;
  });

  const items: SessionRowItem[] = useMemo(
    () =>
      (rows ?? []).map((s) => ({
        sessionId: s.sessionId,
        instanceId,
        title: s.title,
        // The listing's updatedAt is an ISO string; rows render epoch ms.
        updatedAt: s.updatedAt
          ? Date.parse(s.updatedAt) || undefined
          : undefined,
        workspace: s.cwd,
        status: s.running ? "running" : undefined,
        workflowActive: s.workflowActivity?.active,
      })),
    [rows, instanceId],
  );

  const visible = (projects ?? []).filter((p) =>
    p.workspacePath.toLowerCase().includes(filter.trim().toLowerCase()),
  );

  return (
    <div className="fixed inset-0 z-50 flex items-end justify-center bg-black/60 p-3 pb-[max(var(--safe-bottom),1rem)]">
      <div className="flex max-h-[75vh] w-full max-w-md flex-col overflow-hidden rounded-2xl border border-hairline bg-surface">
        <div className="flex items-center gap-2 px-4 pt-3">
          {selected ? (
            <button
              onClick={back}
              disabled={resuming !== null}
              aria-label={t("files.back")}
              className="-ml-1 flex size-6 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active disabled:opacity-50"
            >
              <ArrowLeft className="size-4" />
            </button>
          ) : (
            <History className="size-4 shrink-0 text-info" />
          )}
          <h2 className="min-w-0 flex-1 truncate text-sm font-semibold text-ink">
            {selected ? projectName(selected) : t("historyDialog.title")}
          </h2>
          <button onClick={onClose} className="text-faint" aria-label="close">
            ✕
          </button>
        </div>
        <p className="px-4 pt-1 text-xs text-faint">
          {selected
            ? resuming
              ? // Resume POST may incubate the project's serve bridge —
                // same visible-terminal-window behavior as session-create.
                t("projectDialog.creatingHint")
              : t("historyDialog.listHint")
            : t("historyDialog.hint")}
        </p>

        {selected ? (
          rows === null ? (
            <div className="min-h-0 flex-1 px-4 py-8">
              {listError ? (
                <div className="flex flex-col items-center gap-3">
                  <p className="rounded-lg bg-warning-wash px-3 py-2 text-xs text-warning ring-1 ring-warning/30">
                    {listError}
                  </p>
                  <button
                    onClick={() => void openProject(selected)}
                    className="rounded-xl bg-raised px-4 py-2 text-xs font-medium text-ink ring-1 ring-inset ring-hairline hover:bg-hover active:bg-active"
                  >
                    {t("historyDialog.retry")}
                  </button>
                </div>
              ) : (
                <div className="flex flex-col items-center gap-2 text-xs text-faint">
                  <Loader2 className="size-4 animate-spin" />
                  {t("historyDialog.waking")}
                </div>
              )}
            </div>
          ) : (
            <>
              {listError && (
                <p className="mx-4 mb-1 shrink-0 rounded-lg bg-warning-wash px-3 py-2 text-xs text-warning ring-1 ring-warning/30">
                  {listError}
                </p>
              )}
              <SessionList
                sessions={items}
                activeKey={null}
                connecting={resuming !== null}
                onSelect={(_instanceId, sessionId) => void resume(sessionId)}
                emptyHint={t("historyDialog.listEmpty")}
                readOnly
                onDelete={(item) => void removeRow(item.sessionId)}
                footer={
                  cursor ? (
                    <button
                      onClick={() => void loadMore()}
                      disabled={loadingMore}
                      className="mx-auto mb-3 mt-1 flex items-center gap-2 rounded-xl bg-raised px-4 py-2 text-xs font-medium text-dim ring-1 ring-inset ring-hairline hover:bg-hover active:bg-active disabled:opacity-50"
                    >
                      {loadingMore && (
                        <Loader2 className="size-3.5 animate-spin" />
                      )}
                      {t(
                        loadingMore
                          ? "historyDialog.loadingMore"
                          : "historyDialog.loadMore",
                      )}
                    </button>
                  ) : items.length > 0 ? (
                    <p className="py-3 text-center text-2xs text-faint">
                      {t("historyDialog.end")}
                    </p>
                  ) : null
                }
              />
            </>
          )
        ) : (
          <>
            <div className="flex items-center gap-2 px-4 py-2">
              <Search className="size-3.5 shrink-0 text-faint" />
              <input
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
                placeholder={t("historyDialog.filter")}
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
                  {t("historyDialog.loading")}
                </div>
              )}
              {projects && visible.length === 0 && !error && (
                <p className="px-4 py-6 text-center text-xs text-faint">
                  {t("historyDialog.empty")}
                </p>
              )}
              {visible.map((p) => (
                <ProjectRow
                  key={p.workspacePath}
                  project={p}
                  disabled={loadingPage}
                  onOpen={() => void openProject(p.workspacePath)}
                  onLongPress={() => setProjAction(p)}
                />
              ))}
            </div>
          </>
        )}
      </div>

      {projAction && (
        <div
          className="fixed inset-0 z-50 flex items-end justify-center bg-black/60 p-3 pb-[max(var(--safe-bottom),1rem)]"
          onClick={() => (projDeleting ? undefined : setProjAction(null))}
        >
          <div
            className="w-full max-w-md overflow-hidden rounded-2xl border border-hairline bg-surface"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="px-4 pt-4">
              <h2 className="truncate text-sm font-semibold text-ink">
                {projectName(projAction.workspacePath)}
              </h2>
              <p className="mt-1 text-xs text-faint">
                {t("historyDialog.deleteProjectHint")}
              </p>
            </div>
            <div className="flex flex-col gap-2 px-4 pb-4 pt-3">
              <button
                onClick={() => void removeProject(projAction.workspacePath)}
                disabled={projDeleting}
                className="flex items-center justify-center gap-2 rounded-xl bg-danger-wash px-4 py-3 text-sm font-medium text-danger ring-1 ring-inset ring-danger/40 active:brightness-125 disabled:opacity-50"
              >
                {projDeleting && <Loader2 className="size-4 animate-spin" />}
                {t("historyDialog.deleteProject")}
              </button>
              <button
                onClick={() => setProjAction(null)}
                disabled={projDeleting}
                className="rounded-xl bg-raised px-4 py-3 text-sm font-medium text-ink ring-1 ring-inset ring-hairline hover:bg-hover active:bg-active disabled:opacity-50"
              >
                {t("common.cancel")}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// Chooser row: tap opens the project, long-press offers its delete — the
// same gesture + sheet pattern as the session rows.
function ProjectRow({
  project,
  disabled,
  onOpen,
  onLongPress,
}: {
  project: HubProject;
  disabled?: boolean;
  onOpen: () => void;
  onLongPress: () => void;
}) {
  const { t, i18n } = useTranslation();
  const press = useLongPress(onLongPress);
  return (
    <button
      onClick={onOpen}
      disabled={disabled}
      {...press}
      className="mb-1 flex w-full flex-col items-start gap-0.5 rounded-xl px-3 py-2.5 text-left select-none [-webkit-touch-callout:none] hover:bg-hover active:bg-active disabled:opacity-50"
    >
      <span className="flex w-full items-center gap-2">
        <span className="min-w-0 flex-1 truncate text-sm text-ink">
          {projectName(project.workspacePath)}
        </span>
        <span className="shrink-0 text-2xs text-faint">
          {t("projectDialog.sessionCount", { count: project.sessions })}
        </span>
      </span>
      <span className="flex w-full items-center gap-2 text-2xs text-faint">
        <span className="min-w-0 flex-1 truncate font-mono">
          {project.workspacePath}
        </span>
        <span className="shrink-0">
          {fmtRelative(project.lastActive, i18n.language)}
        </span>
      </span>
    </button>
  );
}
