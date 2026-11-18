import { useState } from "react";
import { useTranslation } from "react-i18next";
import { RotateCcw } from "lucide-react";
import { useAppStore } from "../../store/appStore";
import {
  ConfigBlock,
  ConfigEmpty,
  ConfigPageFrame,
  fmtStamp,
} from "../../components/config/ConfigPage";

// Config backups (ADR-0009). The bridge keeps a timestamped copy before each
// rewrite of the two files it owns, so a restore is "put that copy back".
//
// Scoped per file, which is worth stating on screen: restoring the provider
// config does not also restore the CLI config, so a user who broke both has
// to restore both. The effect class comes back with the answer — a cli-config
// restore needs a restart, a provider-config one does not — and the store
// surfaces it rather than this screen guessing.
interface Backup {
  path: string;
  createdAt: string;
}

export function BackupsPage() {
  const { t } = useTranslation();
  const backups = useAppStore((s) => s.configBackups);
  const supported = useAppStore((s) => s.configSupported);
  const loading = useAppStore((s) => s.configLoading);
  const error = useAppStore((s) => s.configError);
  const loadConfigSection = useAppStore((s) => s.loadConfigSection);
  const applyConfigWrite = useAppStore((s) => s.applyConfigWrite);
  const restoreBackup = useAppStore((s) => s.restoreBackup);
  const [confirm, setConfirm] = useState<{ file: string; path: string } | null>(null);

  const payload = backups as
    | { ok?: boolean; backups?: { providerConfig?: Backup[]; cliConfig?: Backup[] } }
    | null;

  const groups: Array<{ key: "providerConfig" | "cliConfig"; label: string; items: Backup[] }> =
    [
      {
        key: "providerConfig",
        label: t("zconfig.backupProvider"),
        items: payload?.backups?.providerConfig ?? [],
      },
      {
        key: "cliConfig",
        label: t("zconfig.backupCli"),
        items: payload?.backups?.cliConfig ?? [],
      },
    ];

  async function restore(file: string, path: string) {
    setConfirm(null);
    // Reload both files: a restore rewrites the whole document, so the
    // snapshot the other screens hold is stale either way.
    await applyConfigWrite("restore backup", () => restoreBackup(file, path), [
      "models",
      "mcp",
      "hooks",
      "skills",
      "agents",
    ]);
  }

  const total = groups.reduce((n, g) => n + g.items.length, 0);

  return (
    <ConfigPageFrame
      title={t("zconfig.backups")}
      onBack={() => useAppStore.getState().openConfig(null)}
      onRefresh={() => void loadConfigSection("backups")}
      refreshing={loading}
      unsupported={supported === false}
      error={error}
      hasLoaded={backups !== null}
    >
      {total === 0 ? (
        <ConfigEmpty text={t("zconfig.backupsEmpty")} />
      ) : (
        groups.map((g, gi) => (
          <ConfigBlock key={g.key} title={g.label} divided={gi > 0}>
            {g.items.length === 0 ? (
              <p className="px-4 pb-3 text-xs text-faint">
                {t("zconfig.backupsNoneForFile")}
              </p>
            ) : (
              g.items.map((b) => {
                const ms = Date.parse(b.createdAt);
                const isConfirming = confirm?.path === b.path;
                return (
                  <div key={b.path} className="px-4 py-3">
                    <div className="flex items-baseline justify-between gap-3">
                      <span className="text-sm tabular-nums text-dim">
                        {Number.isNaN(ms) ? b.createdAt : fmtStamp(ms)}
                      </span>
                      {isConfirming ? (
                        <span className="flex shrink-0 items-center gap-1.5">
                          <button
                            onClick={() => setConfirm(null)}
                            className="rounded-lg px-2 py-1 text-2xs text-faint"
                          >
                            {t("common.cancel")}
                          </button>
                          <button
                            onClick={() => void restore(g.key, b.path)}
                            className="rounded-lg bg-warning-wash px-2.5 py-1 text-2xs font-medium text-warning"
                          >
                            {t("zconfig.confirmRestore")}
                          </button>
                        </span>
                      ) : (
                        <button
                          onClick={() => setConfirm({ file: g.key, path: b.path })}
                          className="flex shrink-0 items-center gap-1.5 rounded-lg bg-hover px-2.5 py-1.5 text-2xs text-dim active:bg-active"
                        >
                          <RotateCcw className="size-3" />
                          {t("zconfig.restore")}
                        </button>
                      )}
                    </div>
                    <p className="pt-0.5 font-mono text-2xs break-all text-faint">
                      {b.path}
                    </p>
                  </div>
                );
              })
            )}
          </ConfigBlock>
        ))
      )}

      <p className="px-4 pt-3 text-center text-2xs text-faint">
        {t("zconfig.backupsScope")}
      </p>
    </ConfigPageFrame>
  );
}
