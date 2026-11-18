import { useTranslation } from "react-i18next";
import { Copy } from "lucide-react";
import { useAppStore } from "../../store/appStore";
import {
  ConfigBlock,
  ConfigEmpty,
  ConfigPageFrame,
} from "../../components/config/ConfigPage";

// Discovered skills (ADR-0009): every SKILL.md the bridge found, with its
// enabled state and where it lives.
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

  return (
    <ConfigPageFrame
      title={t("zconfig.skills")}
      onBack={() => useAppStore.getState().openConfig(null)}
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
            // user tree — and for a project skill it is the one way to get
            // an editable personal copy.
            const copyable = scope != null && !isUserTreeScope(scope);
            return (
              <div key={path} className="px-4 py-3">
                <div className="flex items-start gap-3">
                  <span className="min-w-0 flex-1">
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
                  </span>
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
