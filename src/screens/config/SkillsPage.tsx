import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { Copy } from "lucide-react";
import { useAppStore } from "../../store/appStore";
import {
  ConfigBlock,
  ConfigEmpty,
  ConfigPageFrame,
} from "../../components/config/ConfigPage";

// Discovered skills (ADR-0009): every SKILL.md the bridge found, with its
// enabled state and where it lives. A row opens the skill's detail (the file
// as written — frontmatter plus body, read-only); the reversible controls
// stay on the rows.
//
// The API can delete a skill, but a list row is the wrong place to decide
// which file on somebody's machine stops existing — so this screen offers
// only the reversible controls: the enable switch (immediate) and, for skills
// that live outside the user's own tree, a copy into it (which produces an
// editable copy without touching the original).
//
// Scope, reported directly by the bridge (`user` | `agents` | `plugin` |
// `project`):
//   - user / agents: the user's own trees — copying there is pointless.
//   - plugin: shipped with a plugin — a copy would desync the plugin install.
//   - project: a workspace skill — copying is the one way to get an editable
//     personal version.

/** Scope values the bridge reports (server skills.ts SkillEntry). */
export type SkillScope = "user" | "agents" | "plugin" | "project";

const USER_TREE_SCOPES = new Set<SkillScope>(["user", "agents"]);

/** Whether a skill already lives in one of the user's own trees. */
export function isUserTreeScope(scope: SkillScope | undefined): boolean {
  return scope !== undefined && USER_TREE_SCOPES.has(scope);
}

/** One skill's detail (GET /settings/skills/{path}, server readSkillDetail):
 * every frontmatter key as written plus the SKILL.md body after the block.
 * Fields are optional at the type level — the wire is permissive. */
export interface SkillDetailBody {
  name?: string;
  description?: string;
  metadata?: Record<string, string>;
  body?: string;
  path?: string;
  dir?: string;
  scope?: SkillScope;
  enabled?: boolean;
  deletable?: boolean;
}

/** Decode the detail route's `{ok, skill}`. A body without a skill object
 * reads as null — the view treats that as a load failure, never as a fake
 * empty skill. */
export function decodeSkillDetail(payload: unknown): SkillDetailBody | null {
  const body = payload as { skill?: SkillDetailBody } | null;
  return body?.skill ?? null;
}

export function SkillsPage() {
  const { t } = useTranslation();
  const skills = useAppStore((s) => s.configSkills);
  const supported = useAppStore((s) => s.configSupported);
  const loading = useAppStore((s) => s.configLoading);
  const error = useAppStore((s) => s.configError);
  const loadConfigSection = useAppStore((s) => s.loadConfigSection);
  const applyConfigWrite = useAppStore((s) => s.applyConfigWrite);
  const setSkillEnabled = useAppStore((s) => s.setSkillEnabled);
  const copySkillToUser = useAppStore((s) => s.copySkillToUser);
  const detailPath = useAppStore((s) => s.configSkillDetailPath);
  const rawDetail = useAppStore((s) => s.configSkillDetail);
  const detailError = useAppStore((s) => s.configSkillDetailError);
  const loadSkillDetail = useAppStore((s) => s.loadSkillDetail);
  const clearSkillDetail = useAppStore((s) => s.clearSkillDetail);

  // Re-entering the page always re-enters the list — a detail left open by
  // the last visit would paint stale data.
  useEffect(() => {
    clearSkillDetail();
  }, [clearSkillDetail]);

  const payload = skills as {
    ok?: boolean;
    skills?: Array<{
      path?: string;
      name?: string;
      description?: string;
      scope?: SkillScope;
      enabled?: boolean;
    }>;
  } | null;

  const rows = payload?.skills ?? [];

  async function toggle(path: string, enable: boolean) {
    await applyConfigWrite(
      enable ? "enable skill" : "disable skill",
      () => setSkillEnabled(path, enable),
      ["skills"],
    );
  }

  async function copyToUser(path: string) {
    await applyConfigWrite("copy skill", () => copySkillToUser(path), [
      "skills",
    ]);
  }

  // The detail sub-view replaces the list under the same frame: its back
  // gesture returns here, and its refresh re-reads the file.
  if (detailPath !== null) {
    const detail = decodeSkillDetail(rawDetail);
    return (
      <ConfigPageFrame
        title={detail?.name || detail?.path || detailPath}
        onBack={clearSkillDetail}
        onRefresh={() => void loadSkillDetail(detailPath)}
        refreshing={false}
        loading={rawDetail === null && detailError === null}
        hasLoaded={rawDetail !== null || detailError !== null}
        unsupported={supported === false}
        error={null}
      >
        {detailError ? (
          <div className="mx-4 mt-2 rounded-xl bg-surface px-4 py-6 text-center ring-1 ring-hairline">
            <p className="text-sm text-dim">
              {t("zconfig.skillDetailLoadFailed")}
            </p>
            <p className="mt-1.5 break-all text-xs text-faint">{detailError}</p>
          </div>
        ) : (
          detail && <SkillDetailView detail={detail} />
        )}
      </ConfigPageFrame>
    );
  }

  return (
    <ConfigPageFrame
      title={t("zconfig.skills")}
      onBack={() => useAppStore.getState().backFromConfigSection()}
      onRefresh={() => void loadConfigSection("skills")}
      refreshing={loading}
      unsupported={supported === false}
      error={error}
      hasLoaded={skills !== null}
    >
      {rows.length === 0 ? (
        <ConfigEmpty text={t("zconfig.skillsEmpty")} />
      ) : (
        <ConfigBlock>
          {rows.map((s) => {
            const path = s.path ?? s.name ?? "";
            const scope = s.scope;
            // Copy only makes sense for a skill that is NOT already in the
            // user tree — and for a project skill it is the one way to get an
            // editable personal copy.
            const copyable = scope != null && !isUserTreeScope(scope);
            return (
              <div key={path} className="px-4 py-3">
                <div className="flex items-start gap-3">
                  {/* The row's text opens the detail (the file as written);
                      the controls stay beside it. */}
                  <button
                    onClick={() => void loadSkillDetail(path)}
                    className="min-w-0 flex-1 text-left active:bg-active"
                  >
                    <span className="block text-sm text-ink">
                      {s.name ?? path}
                    </span>
                    {s.description && (
                      <span className="block truncate text-2xs text-faint">
                        {s.description}
                      </span>
                    )}
                    <span className="block truncate font-mono text-2xs text-faint">
                      {scope ? `${t(`zconfig.scope${scope}`)} · ` : ""}
                      {path}
                    </span>
                  </button>
                  {/* Immediate effect, so a switch is the right control — no
                      confirm, no restart. */}
                  <button
                    role="switch"
                    aria-checked={s.enabled !== false}
                    aria-label={t("zconfig.skillEnabled")}
                    onClick={() => void toggle(path, s.enabled === false)}
                    className={`mt-0.5 h-6 w-10 shrink-0 rounded-full transition ${
                      s.enabled !== false
                        ? "bg-success/80"
                        : "bg-active"
                    }`}
                  >
                    <span
                      className={`block size-5 rounded-full bg-white transition ${
                        s.enabled !== false
                          ? "translate-x-4.5"
                          : "translate-x-0.5"
                      }`}
                    />
                  </button>
                </div>

                {copyable && (
                  <div className="mt-2">
                    <button
                      onClick={() => void copyToUser(path)}
                      className="flex items-center gap-1.5 rounded-lg bg-hover px-2.5 py-1.5 text-2xs text-dim active:bg-active"
                    >
                      <Copy className="size-3" />
                      {t("zconfig.skillCopy")}
                    </button>
                  </div>
                )}
              </div>
            );
          })}
        </ConfigBlock>
      )}
    </ConfigPageFrame>
  );
}

/** The detail body: scope + enabled line, description, the file path, every
 * frontmatter key as written, then the SKILL.md body — read-only. */
function SkillDetailView({ detail }: { detail: SkillDetailBody }) {
  const { t } = useTranslation();
  const metadata = Object.entries(detail.metadata ?? {});
  return (
    <ConfigBlock>
      <div className="flex flex-col gap-2 px-4 py-4">
        <p className="text-2xs text-faint">
          {detail.scope ? `${t(`zconfig.scope${detail.scope}`)} · ` : ""}
          {detail.enabled !== false
            ? t("zconfig.skillOn")
            : t("zconfig.skillOff")}
        </p>
        {detail.description ? (
          <p className="text-xs text-dim">{detail.description}</p>
        ) : null}
        {detail.path ? (
          <p className="break-all font-mono text-2xs text-faint">
            {detail.path}
          </p>
        ) : null}
      </div>
      {metadata.length > 0 && (
        <div className="border-t border-hairline">
          <h2 className="px-4 pb-1 pt-3 text-2xs font-medium uppercase tracking-wide text-faint">
            {t("zconfig.skillDetailMetadata")}
          </h2>
          {metadata.map(([key, value]) => (
            <div key={key} className="flex items-baseline gap-3 px-4 py-1.5">
              <span className="w-32 shrink-0 truncate font-mono text-2xs text-faint">
                {key}
              </span>
              <span className="min-w-0 flex-1 break-all font-mono text-2xs text-ink">
                {value}
              </span>
            </div>
          ))}
        </div>
      )}
      {detail.body ? (
        <div className="border-t border-hairline">
          <h2 className="px-4 pb-1 pt-3 text-2xs font-medium uppercase tracking-wide text-faint">
            {t("zconfig.skillDetailBody")}
          </h2>
          <pre className="whitespace-pre-wrap break-words px-4 pb-4 font-mono text-2xs leading-relaxed text-ink">
            {detail.body}
          </pre>
        </div>
      ) : null}
    </ConfigBlock>
  );
}
