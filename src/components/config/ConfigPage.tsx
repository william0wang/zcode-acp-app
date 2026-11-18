import { type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { ArrowLeft, RefreshCw } from "lucide-react";
import { useBackHandler } from "../../lib/backNav";
import { useAppStore } from "../../store/appStore";

// Shared frame for every configuration section page (ADR-0009): a back
// header, an optional refresh, and the three states a section can be in —
// loading, unsupported (an older hub), or loaded. Keeping them here means a
// new section page only writes its own content.
export function ConfigPageFrame({
  title,
  onBack,
  onRefresh,
  refreshing,
  loading,
  unsupported,
  error,
  children,
  actions,
  hasLoaded,
}: {
  title: string;
  onBack: () => void;
  onRefresh?: () => void;
  refreshing?: boolean;
  loading?: boolean;
  unsupported?: boolean;
  error?: string | null;
  children: ReactNode;
  // Extra header controls (a restart affordance, a section filter).
  actions?: ReactNode;
  /**
   * Whether this section's payload has ever landed. The spinner shows only
   * while a load is in flight AND nothing has been shown yet — a refresh over
   * an already-populated list keeps the list on screen instead of blanking it.
   * Pass `false` on a section that reads the shared snapshot (`/settings/all`),
   * whose payload is already in the store before this frame first renders.
   */
  hasLoaded?: boolean;
}) {
  const { t } = useTranslation();
  // Every configuration page renders this frame, so the gesture mirrors its
  // back button everywhere: a section page returns to the entry list, the
  // entry list closes the configuration screen.
  useBackHandler(() => {
    onBack();
    return true;
  });
  // The configuration screens replace the chat/picker screens, which are the
  // only two that render the notice banner — so the frame renders it itself,
  // otherwise an effect-class or reset-card outcome raised inside a section
  // would be written to a banner nobody can see.
  const notice = useAppStore((s) => s.notice);
  const dismissNotice = useAppStore((s) => s.dismissNotice);
  // Read from the store rather than a prop: every section page renders this
  // frame, and not one of them passed `loading`, so the spinner branch below
  // was dead code and a first load flashed its empty state ("no models",
  // "nothing on this machine") over data that had simply not arrived yet.
  const storeLoading = useAppStore((s) => s.configLoading);
  const storeLoaded = useAppStore((s) => s.configAll !== null);
  const isLoading = loading ?? storeLoading;
  // Default true: a section whose payload comes from the shared snapshot has
  // data before its first render, so it never shows the spinner. A section that
  // fetches its own payload passes `hasLoaded={false}` while that payload is
  // still null, which is what makes the first load spin instead of flashing an
  // empty state.
  const payloadLanded = hasLoaded ?? storeLoaded;
  const noticeText = notice
    ? notice.startsWith("notice.")
      ? t(notice)
      : notice
    : null;

  return (
    <div className="flex h-full flex-col bg-canvas text-ink">
      <header className="flex shrink-0 items-center gap-1 px-3 pb-2 pt-[max(var(--safe-top),0.75rem)]">
        <button
          onClick={onBack}
          aria-label={t("common.close")}
          className="flex size-9 shrink-0 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active"
        >
          <ArrowLeft className="size-4.5" />
        </button>
        <h1 className="min-w-0 flex-1 truncate text-base font-semibold">
          {title}
        </h1>
        {actions}
        {onRefresh && (
          <button
            onClick={onRefresh}
            aria-label={t("quota.refresh")}
            className="flex size-9 shrink-0 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active"
          >
            <RefreshCw
              className={`size-4 ${refreshing ? "animate-spin" : ""}`}
            />
          </button>
        )}
      </header>

      {noticeText && (
        <div className="mx-4 mb-2 flex shrink-0 items-center justify-between gap-2 rounded-lg bg-hover px-3 py-2 text-xs text-dim">
          <span className="min-w-0 flex-1">{noticeText}</span>
          <button onClick={dismissNotice} className="shrink-0 text-faint">
            ✕
          </button>
        </div>
      )}

      {unsupported ? (
        // A capability gap, not a failure: the hub predates the settings API.
        <div className="mx-4 mt-2 rounded-xl bg-surface px-4 py-6 text-center ring-1 ring-hairline">
          <p className="text-sm text-dim">{t("zconfig.unsupported")}</p>
          <p className="mt-1.5 text-xs text-faint">
            {t("zconfig.unsupportedHint")}
          </p>
        </div>
      ) : error ? (
        <div className="mx-4 mt-2 rounded-xl bg-surface px-4 py-6 text-center ring-1 ring-hairline">
          <p className="text-sm text-dim">{t("zconfig.loadFailed")}</p>
          <p className="mt-1.5 break-all text-xs text-faint">{error}</p>
        </div>
      ) : isLoading && !payloadLanded ? (
        <div className="flex flex-1 items-center justify-center">
          <RefreshCw className="size-5 animate-spin text-faint" />
        </div>
      ) : (
        <div className="flex-1 overflow-y-auto pb-[max(var(--safe-bottom),1rem)]">
          {children}
        </div>
      )}
    </div>
  );
}

// One grouped block of rows. `divided` draws the top rule the side panels use
// between sections, so a page of several blocks reads the same way.
export function ConfigBlock({
  title,
  divided,
  children,
}: {
  title?: string;
  divided?: boolean;
  children: ReactNode;
}) {
  return (
    <div className={divided ? "mt-2 border-t border-hairline" : ""}>
      {title && (
        <h2 className="px-4 pb-1 pt-3 text-2xs font-medium uppercase tracking-wide text-faint">
          {title}
        </h2>
      )}
      {children}
    </div>
  );
}

// A read-only label/value row, the shape most configuration data takes.
export function ConfigRow({
  label,
  value,
  hint,
}: {
  label: string;
  value: ReactNode;
  hint?: string;
}) {
  return (
    <div className="px-4 py-2.5">
      <div className="flex items-baseline justify-between gap-3">
        <span className="shrink-0 text-sm text-dim">{label}</span>
        <span className="min-w-0 truncate text-right text-sm text-ink">
          {value}
        </span>
      </div>
      {hint && <p className="pt-0.5 text-xs text-faint">{hint}</p>}
    </div>
  );
}

// Empty-state line for a section with nothing in it.
export function ConfigEmpty({ text }: { text: string }) {
  return <p className="px-4 py-8 text-center text-sm text-faint">{text}</p>;
}

/** Local `MM-DD HH:MM` — the layout the rest of the app stamps times in. */
export function fmtStamp(ms: number): string {
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return "";
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** Compact token count: `1.2M`, `34k`, `870`. */
export function fmtTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1000) return `${(n / 1000).toFixed(n >= 10_000 ? 0 : 1)}k`;
  return String(n);
}
