import { useEffect, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { Activity, ChevronRight, Pencil, RefreshCw, X } from "lucide-react";
import { useBackHandler } from "../lib/backNav";
import { useAppStore } from "../store/appStore";
import { QuotaSection } from "./QuotaSection";

// Shared right slide-over frame. What goes inside depends on where it opens:
// global settings on the entry screen, session-scoped info in chat.
// `action` adds an icon button slot in the header, left of the close button.
export function PanelShell({
  title,
  onClose,
  action,
  children,
}: {
  title: string;
  onClose: () => void;
  action?: ReactNode;
  children: ReactNode;
}) {
  const { t } = useTranslation();
  useBackHandler(() => {
    onClose();
    return true;
  });
  return (
    <div
      className="fixed inset-0 z-40 flex justify-end bg-black/50"
      onClick={onClose}
    >
      <aside
        className="flex h-full w-80 max-w-[85%] flex-col overflow-y-auto border-l border-hairline bg-surface pt-[max(var(--safe-top),0.75rem)] text-ink"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between px-4 pb-2">
          <h2 className="text-sm font-semibold">{title}</h2>
          <div className="flex items-center gap-1">
            {action}
            <button
              onClick={onClose}
              aria-label={t("common.close")}
              className="flex size-8 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active"
            >
              <X className="size-4" />
            </button>
          </div>
        </div>
        {children}
      </aside>
    </div>
  );
}

// Global settings, shown OUTSIDE any session (entry screen): an entry row
// into the server manager (switch/add/edit live in ConnectScreen), the hub
// upgrade trigger, and the account quota card (connection-level data — the
// list screen keeps the instance WS alive, so it renders here too).
// Rarely-touched language/font preferences sit at the very bottom.
// Session-scoped controls live in SessionPanel. ZCode configuration has its
// own header button on the picker — it does not get a second entry here.
export function SettingsPanel({ onClose }: { onClose: () => void }) {
  const { t, i18n } = useTranslation();
  const savedServers = useAppStore((s) => s.savedServers);
  const activeServerId = useAppStore((s) => s.activeServerId);
  const openServerManager = useAppStore((s) => s.openServerManager);
  const setLang = useAppStore((s) => s.setLang);
  const fontSize = useAppStore((s) => s.fontSize);
  const setFontSize = useAppStore((s) => s.setFontSize);
  const upgradeHub = useAppStore((s) => s.upgradeHub);
  const [upgradeBusy, setUpgradeBusy] = useState(false);
  const [upgradeNote, setUpgradeNote] = useState<string | null>(null);

  // Same for the upgrade result note.
  useEffect(() => {
    if (!upgradeNote) return;
    const id = setTimeout(() => setUpgradeNote(null), 5000);
    return () => clearTimeout(id);
  }, [upgradeNote]);

  // The hub alone decides whether to restart; this only triggers its check
  // and rides out the respawn (store polls health + refreshes discovery).
  async function checkHubUpgrade() {
    setUpgradeBusy(true);
    setUpgradeNote(null);
    try {
      const r = await upgradeHub();
      setUpgradeNote(
        r.restarting
          ? t("panel.upgradeRestarted", { version: r.diskVersion ?? "" })
          : t("panel.upgradeLatest", { version: r.runningVersion }),
      );
    } catch (e) {
      setUpgradeNote(e instanceof Error ? e.message : String(e));
    } finally {
      setUpgradeBusy(false);
    }
  }

  function switchLang(lang: "en" | "zh-CN") {
    setLang(lang);
    void i18n.changeLanguage(lang);
  }

  const activeServer = savedServers.find((s) => s.id === activeServerId);
  const openConfig = useAppStore((s) => s.openConfig);

  return (
    <PanelShell title={t("panel.title")} onClose={onClose}>
      <div className="space-y-5 px-4 pb-[max(var(--safe-bottom),1rem)] pt-2">
        <div>
          <h3 className="pb-2 text-2xs font-medium uppercase tracking-wide text-faint">
            {t("panel.server")}
          </h3>
          {/* Entry row only — switching/add/edit all happen inside the
              server manager page (ConnectScreen). */}
          <button
            onClick={openServerManager}
            className="flex w-full items-center gap-2 rounded-xl bg-raised px-3 py-2.5 text-left hover:bg-hover active:bg-active"
          >
            <span className="min-w-0 flex-1">
              <span className="block truncate text-sm text-dim">
                {activeServer?.name ?? "—"}
              </span>
              <span className="block truncate font-mono text-2xs text-faint">
                {activeServer?.hubUrl}
              </span>
            </span>
            <Pencil className="size-4 shrink-0 text-faint" />
          </button>
          <button
            onClick={() => void checkHubUpgrade()}
            disabled={upgradeBusy}
            className="mt-2 flex w-full items-center justify-center gap-2 rounded-xl bg-raised px-3 py-2 text-xs font-medium text-dim hover:bg-hover active:bg-active disabled:opacity-60"
          >
            <RefreshCw
              className={`size-3.5 ${upgradeBusy ? "animate-spin" : ""}`}
            />
            {upgradeBusy ? t("panel.upgradeBusy") : t("panel.upgrade")}
          </button>
          {upgradeNote && (
            <p className="pt-1.5 text-center text-2xs text-faint">
              {upgradeNote}
            </p>
          )}
          {/* Machine health of the hub's host — a read-only dashboard, not
              ZCode configuration, so its entry lives here (deep entry into
              the page; Back returns to this panel, never the config list). */}
          <button
            onClick={() => openConfig("system")}
            className="mt-2 flex w-full items-center justify-between gap-2 rounded-xl bg-raised px-3 py-2.5 text-left hover:bg-hover active:bg-active"
          >
            <span className="flex min-w-0 items-center gap-2 text-sm text-dim">
              <Activity className="size-4 shrink-0 text-faint" />
              {t("system.title")}
            </span>
            <ChevronRight className="size-4 shrink-0 text-faint" />
          </button>
        </div>
      </div>

      {/* Sibling, not inside the px-4 wrapper: rows carry their own padding
          and the section's top divider must span the full panel width. */}
      <QuotaSection />

      {/* Rarely-touched preferences sit at the very bottom of the panel. */}
      <div className="space-y-5 px-4 pb-[max(var(--safe-bottom),1rem)] pt-2">
        <div>
          <h3 className="pb-2 text-2xs font-medium uppercase tracking-wide text-faint">
            {t("panel.language")}
          </h3>
          <div className="flex gap-2 rounded-xl bg-raised p-1">
            {(["en", "zh-CN"] as const).map((lang) => (
              <button
                key={lang}
                onClick={() => switchLang(lang)}
                className={`flex-1 rounded-lg px-3 py-1.5 text-xs transition ${
                  i18n.language === lang
                    ? "bg-active font-medium text-ink"
                    : "text-faint"
                }`}
              >
                {lang === "en" ? "English" : "中文"}
              </button>
            ))}
          </div>
        </div>

        <div>
          <h3 className="pb-2 text-2xs font-medium uppercase tracking-wide text-faint">
            {t("panel.fontSize")}
          </h3>
          <div className="flex gap-2 rounded-xl bg-raised p-1">
            {(["small", "medium", "large"] as const).map((size) => (
              <button
                key={size}
                onClick={() => setFontSize(size)}
                className={`flex-1 rounded-lg px-3 py-1.5 text-xs transition ${
                  fontSize === size
                    ? "bg-active font-medium text-ink"
                    : "text-faint"
                }`}
              >
                {t(
                  `panel.fontSize${size.charAt(0).toUpperCase() + size.slice(1)}`,
                )}
              </button>
            ))}
          </div>
        </div>
      </div>

      <p className="px-4 pb-[max(var(--safe-bottom),1rem)] pt-4 text-center text-2xs text-faint">
        v{__APP_VERSION__}
      </p>
    </PanelShell>
  );
}
