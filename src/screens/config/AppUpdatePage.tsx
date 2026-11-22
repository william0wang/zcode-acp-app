import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Download, RefreshCw } from "lucide-react";
import { useAppStore } from "../../store/appStore";
import {
  ConfigBlock,
  ConfigEmpty,
  ConfigPageFrame,
  ConfigRow,
} from "../../components/config/ConfigPage";
import type { AppUpdateStage } from "../../lib/types";

// App update (ADR-0009). The bridge downloads and verifies the artifact
// itself; what the user sees here is the outcome, which is not always success:
//
//  - `done`: the bundle on disk is the new one — quit and reopen the app.
//  - `needs-user-install`: the download and checksum passed but the install
//    location refused the swap (no password prompt, no Finder automation). The
//    verified bundle is left at `artifactPath` for the user to move by hand.
//    That is an outcome, not a failure, and the screen must not call it one.
//  - `failed`: the error string says which step gave up.
//
// Progress is polled rather than pushed: the download runs in the bridge, and
// the only channel back is the same GET that reports the check.
export function AppUpdatePage() {
  const { t } = useTranslation();
  const appUpdate = useAppStore((s) => s.configAppUpdate);
  const install = useAppStore((s) => s.appUpdateInstall);
  const supported = useAppStore((s) => s.configSupported);
  const loading = useAppStore((s) => s.configLoading);
  const error = useAppStore((s) => s.configError);
  const loadConfigSection = useAppStore((s) => s.loadConfigSection);
  const installAppUpdate = useAppStore((s) => s.installAppUpdate);
  const pollAppUpdate = useAppStore((s) => s.pollAppUpdate);
  const updateChannel = useAppStore((s) => s.updateChannel);
  const setUpdateChannel = useAppStore((s) => s.setUpdateChannel);
  const [confirm, setConfirm] = useState(false);

  const check = appUpdate as
    | {
        ok?: boolean;
        appUpdate?: {
          updateAvailable?: boolean;
          currentVersion?: string | null;
          latestVersion?: string | null;
          channel?: string;
          platform?: string;
          appPath?: string | null;
          releaseName?: string | null;
          releaseNotes?: string | null;
          files?: Array<{ url: string; sha512?: string; size?: number }>;
          install?: AppUpdateStage;
        };
      }
    | null;

  const info = check?.appUpdate;
  const artifact = info?.files?.[0];
  const stage = install?.stage ?? "idle";
  const busy = stage === "downloading" || stage === "installing";

  // Poll while a download runs; stop the moment it settles. The 2s cadence is
  // coarse on purpose — the bridge reports byte counts, and a phone polling
  // that hard for a progress bar is not worth the traffic.
  useEffect(() => {
    if (!busy) return;
    const id = setInterval(() => void pollAppUpdate(), 2000);
    return () => clearInterval(id);
  }, [busy, pollAppUpdate]);

  async function startInstall() {
    setConfirm(false);
    if (!info?.latestVersion || !artifact) return;
    // Name the channel explicitly: it is the stream this very screen was
    // checked against, and an install that silently fell back to stable would
    // be refused by the route for not being the latest stable version.
    await installAppUpdate({
      version: info.latestVersion,
      url: artifact.url,
      channel: updateChannel,
    });
  }

  const pct =
    install?.totalBytes && install.totalBytes > 0
      ? Math.min(100, Math.round((install.receivedBytes / install.totalBytes) * 100))
      : null;

  return (
    <ConfigPageFrame
      title={t("zconfig.appUpdate")}
      onBack={() => useAppStore.getState().backFromConfigSection()}
      onRefresh={() => void loadConfigSection("appUpdate")}
      refreshing={loading}
      unsupported={supported === false}
      error={error}
      hasLoaded={appUpdate !== null}
    >
      <ConfigBlock>
        <ConfigRow
          label={t("zconfig.updateInstalled")}
          value={info?.currentVersion ?? "—"}
        />
        <ConfigRow label={t("zconfig.updateLatest")} value={info?.latestVersion ?? "—"} />
        {info?.platform && (
          <ConfigRow label={t("zconfig.updatePlatform")} value={info.platform} />
        )}
        {/* The channel picker. Both the check and the install must name the
            same stream — the install route re-reads the manifest for the
            channel it is given and refuses anything that is not the latest
            there, so a stable install of a preview version is rejected. */}
        <div className="flex items-center gap-2 px-4 py-2.5">
          <span className="flex-1 text-sm text-dim">
            {t("zconfig.updateChannel")}
          </span>
          <div className="flex gap-1 rounded-lg bg-hover p-0.5">
            {(["stable", "preview"] as const).map((ch) => (
              <button
                key={ch}
                onClick={() => setUpdateChannel(ch)}
                className={`rounded-md px-2.5 py-1 text-2xs transition ${
                  updateChannel === ch
                    ? "bg-active font-medium text-ink"
                    : "text-faint hover:bg-hover active:bg-active"
                }`}
              >
                {t(`zconfig.updateChannel${ch === "stable" ? "Stable" : "Preview"}`)}
              </button>
            ))}
          </div>
        </div>
      </ConfigBlock>

      <ConfigBlock divided>
        {!info?.updateAvailable ? (
          <ConfigEmpty text={t("zconfig.updateNone")} />
        ) : busy ? (
          <div className="px-4 py-4">
            <p className="text-sm text-dim">
              {stage === "installing"
                ? t("zconfig.updateInstalling")
                : t("zconfig.updateDownloading")}
            </p>
            {pct != null && (
              <>
                <div className="mt-2 h-1.5 overflow-hidden rounded-full bg-selected">
                  <div
                    className="h-full rounded-full bg-faint"
                    style={{ width: `${pct}%` }}
                  />
                </div>
                <p className="pt-1 text-2xs tabular-nums text-faint">
                  {pct}%
                </p>
              </>
            )}
            {stage === "installing" && (
              <RefreshCw className="mt-2 size-4 animate-spin text-faint" />
            )}
          </div>
        ) : stage === "done" ? (
          <div className="px-4 py-4">
            <p className="text-sm text-success">
              {t("zconfig.updateDone", { version: info.latestVersion ?? "" })}
            </p>
            <p className="pt-1 text-xs text-faint">{t("zconfig.updateRestartApp")}</p>
          </div>
        ) : stage === "needs-user-install" ? (
          <div className="px-4 py-4">
            <p className="text-sm text-warning">{t("zconfig.updateNeedsUser")}</p>
            {install?.artifactPath && (
              <p className="pt-1 font-mono text-2xs break-all text-faint">
                {install.artifactPath}
              </p>
            )}
            <p className="pt-1.5 text-xs text-faint">
              {t("zconfig.updateNeedsUserHint")}
            </p>
          </div>
        ) : stage === "failed" ? (
          <div className="px-4 py-4">
            <p className="text-sm text-danger">{t("zconfig.updateFailed")}</p>
            {install?.error && (
              <p className="pt-1 font-mono text-2xs break-all text-faint">
                {install.error}
              </p>
            )}
          </div>
        ) : confirm ? (
          <div className="px-4 py-4">
            <p className="text-xs text-dim">
              {t("zconfig.updateConfirm", { version: info.latestVersion ?? "" })}
            </p>
            <div className="mt-2 flex gap-2">
              <button
                onClick={() => setConfirm(false)}
                className="flex-1 rounded-lg px-3 py-2 text-xs text-faint hover:bg-hover active:bg-active"
              >
                {t("common.cancel")}
              </button>
              <button
                onClick={() => void startInstall()}
                className="flex-1 rounded-lg bg-active px-3 py-2 text-xs font-medium text-ink active:bg-active"
              >
                {t("zconfig.updateInstall")}
              </button>
            </div>
          </div>
        ) : (
          <div className="px-4 py-4">
            <button
              onClick={() => setConfirm(true)}
              className="flex w-full items-center justify-center gap-2 rounded-lg bg-selected px-3 py-2.5 text-xs font-medium text-ink hover:bg-hover active:bg-active"
            >
              <Download className="size-3.5" />
              {t("zconfig.updateInstall")}
            </button>
            <p className="pt-2 text-2xs text-faint">
              {t("zconfig.updateSize", {
                size: artifact?.size
                  ? `${Math.round(artifact.size / 1_000_000)}MB`
                  : "—",
              })}
            </p>
          </div>
        )}
      </ConfigBlock>

      {info?.releaseNotes && (
        <ConfigBlock title={t("zconfig.updateNotes")} divided>
          <p className="px-4 py-2 text-xs whitespace-pre-wrap text-dim">
            {info.releaseNotes}
          </p>
        </ConfigBlock>
      )}
    </ConfigPageFrame>
  );
}
