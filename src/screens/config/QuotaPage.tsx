import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, RefreshCw } from "lucide-react";
import { useAppStore } from "../../store/appStore";
import {
  ConfigBlock,
  ConfigEmpty,
  ConfigPageFrame,
  fmtStamp,
} from "../../components/config/ConfigPage";
import { QuotaRow } from "../../components/QuotaSection";

// Plan quota with its reset cards (ADR-0009). The side panels' quota card is
// read-only and untouched; this screen is where a card can actually be spent.
//
// Spending a card is the one irreversible action in the settings API, so the
// gesture always ends in a confirmation naming the card's window and expiry.
// Below the usage threshold that confirmation is PRECEDED by a warning step
// ("usage has not reached X% — continue?"): the card stays usable, but using
// it well before the allowance runs low is a decision worth double-checking.
// The threshold is read on the card's OWN window — a 5-hour card against the
// 5-hour window, a weekly card against the weekly one — because a card cannot
// clear a window it does not reset. Between the confirmations and the spend
// sits an opportunity request, because the backend can decline one ("not
// now") and a declined spend is a wasted round trip.
export const SPEND_THRESHOLD_PCT = 90;

/** The two card windows the reset API sells. */
type ResetType = "FIVE_HOUR" | "WEEK";

/**
 * One spend gesture in flight: which window, and whether the below-threshold
 * warning has been answered. `warned: false` renders the warning step; `true`
 * renders the spend confirmation. At or above the threshold a gesture starts
 * already warned — its single confirmation is the spend itself.
 */
interface SpendStage {
  type: ResetType;
  warned: boolean;
}

/**
 * The quota item each card window clears (server quota/parse.ts deriveLabel:
 * GLM token limits keyed `token_5h` for the 5-hour window, `token_week` for
 * the weekly one).
 */
const WINDOW_KEY_OF: Record<ResetType, string> = {
  FIVE_HOUR: "token_5h",
  WEEK: "token_week",
};

/** The provider ids that can own a coding plan (server coding-plan.ts). */
export const CODING_PLAN_PROVIDERS = [
  "account:bigmodel-individual-coding-plan",
  "account:bigmodel-team-coding-plan",
  "account:zai-individual-coding-plan",
  "account:zai-team-coding-plan",
];

// A team plan needs organization/project headers a headless process cannot
// read, so its cards can never be spent from here. Say so instead of letting
// the user commit to a request that fails.
export function isTeamPlan(providerId: string): boolean {
  return providerId.includes("-team-");
}

/**
 * The used percentage of the ONE window a card type clears.
 *
 * A card's confirmation count reads its OWN window — an exhausted week must
 * not let a 5-hour card skip its warning, because that card cannot clear the
 * week; spent there it is wasted on a window nowhere near its limit. Only the
 * GLM branch counts (the payload also carries Opencode Go and Ollama windows,
 * which a coding-plan card does nothing for), and a window the payload does
 * not list reads as 0 — unknown usage keeps the warning rather than skipping
 * it. Exported so the page and its tests agree on which window counts.
 */
export function windowUsedPercent(
  usageStats: {
    glm: { items?: Array<{ key: string; usedPercent: number }> };
  } | null,
  type: ResetType,
): number {
  return (
    usageStats?.glm.items?.find((it) => it.key === WINDOW_KEY_OF[type])
      ?.usedPercent ?? 0
  );
}

/**
 * A short label for a coding-plan provider id.
 *
 * The wire ids are long (`account:bigmodel-individual-coding-plan`) and the
 * parts that matter to a human are the family and whether it is a personal or
 * team plan. The picker shows that instead of the raw string.
 */
function providerLabel(id: string, t: (key: string) => string): string {
  const family = id.includes(":zai-") ? "Zai" : "Bigmodel";
  return id.includes("-team-")
    ? `${family} · ${t("zconfig.planTeam")}`
    : `${family} · ${t("zconfig.planIndividual")}`;
}

export function QuotaPage() {
  const { t } = useTranslation();
  const usageStats = useAppStore((s) => s.usageStats);
  const usageStatsAt = useAppStore((s) => s.usageStatsAt);
  const quotaUnavailable = useAppStore((s) => s.quotaUnavailable);
  const refreshUsageStats = useAppStore((s) => s.refreshUsageStats);
  const resetCards = useAppStore((s) => s.resetCards);
  const resetProviderId = useAppStore((s) => s.resetProviderId);
  const resetBusy = useAppStore((s) => s.resetBusy);
  const resetNextTryAt = useAppStore((s) => s.resetNextTryAt);
  const supported = useAppStore((s) => s.configSupported);
  const loadResetCards = useAppStore((s) => s.loadResetCards);
  const spendResetCard = useAppStore((s) => s.spendResetCard);
  const markResetHistoryRead = useAppStore((s) => s.markResetHistoryRead);
  const [confirm, setConfirm] = useState<SpendStage | null>(null);

  // Which providers could hold cards, from the entry snapshot's eligibility
  // check (a provider-id shape test plus a credential-file check). The hub
  // lists all four ids regardless of the account, so the picker is the norm
  // rather than the exception.
  const eligible = useAppStore((s) => s.resetEligible);
  const credentialsOk = useAppStore((s) => s.resetCredentials);
  // Nothing is pre-selected: guessing which plan the account holds would show
  // an empty inventory and read as "I have no cards".
  const providerId = resetProviderId;

  // Load the inventory once the provider is known. The nonce it returns is
  // what the spend sends back, so a screen that sat open for an hour cannot
  // burn a card from stale state.
  useEffect(() => {
    if (providerId) void loadResetCards(providerId);
  }, [providerId, loadResetCards]);

  // The unread badge is cosmetic, but clearing it on the way out means the
  // user is not shown a dot for history they just read.
  useEffect(() => {
    if (resetCards?.hasUnreadHistory) void markResetHistoryRead();
  }, [resetCards?.hasUnreadHistory, markResetHistoryRead]);

  const teamBlocked = providerId ? isTeamPlan(providerId) : false;

  // A gesture on a window below the threshold opens on the warning step; at
  // or above it the single confirmation is the spend itself. Each card reads
  // its own window, so the two lists can sit on different sides of it.
  function askSpend(type: ResetType) {
    setConfirm({
      type,
      warned: windowUsedPercent(usageStats, type) >= SPEND_THRESHOLD_PCT,
    });
  }

  async function spend(resetType: ResetType) {
    // Capture BOTH halves of the spend now, while the confirm dialog is still
    // open and the user cannot switch provider underneath. Re-reading the store
    // inside the action would let a provider switch redirect the spend at
    // another plan's card — spending is irreversible, so the wrong provider is
    // the one failure mode worth engineering against.
    const spendProviderId = providerId;
    const spendNonce = resetCards?.nonce ?? "";
    if (!spendProviderId || !spendNonce) return;
    setConfirm(null);
    // Spend straight off the status inventory, the way the desktop app does.
    // /opportunity is the GRANT endpoint: while an unconsumed card is held its
    // per-type issuance cap answers "denied", so gating a spend on a fresh
    // grant made every spend on an available card abort before /use ever ran
    // ("后端拒绝了这次重置"). The desktop never asks opportunity before use —
    // availability in /status is the only gate.
    await spendResetCard({
      providerId: spendProviderId,
      nonce: spendNonce,
      resetType,
    });
  }

  return (
    <ConfigPageFrame
      title={t("zconfig.quota")}
      onBack={() => useAppStore.getState().openConfig(null)}
      onRefresh={() => void refreshUsageStats()}
      unsupported={supported === false}
    >
      {/* ---- the allowance itself ---- */}
      {/* GLM only: this screen is the coding-plan surface (the cards and the
          windows they reset). Other platforms' windows — Opencode Go, Ollama —
          are not part of a coding plan and stay in the side panels' card. */}
      <ConfigBlock>
        {quotaUnavailable ? (
          <ConfigEmpty text={t("quota.unavailable")} />
        ) : usageStats ? (
          usageStats.glm.kind === "success" ? (
            (usageStats.glm.items ?? []).map((it) => (
              <QuotaRow
                key={it.key}
                label={it.label}
                percent={it.usedPercent}
                resetMs={it.nextResetTime}
                used={it.key === "mcp" ? it.usedCount : undefined}
                total={it.key === "mcp" ? it.totalCount : undefined}
              />
            ))
          ) : (
            <ConfigEmpty text={t("quota.unavailable")} />
          )
        ) : (
          <ConfigEmpty text={t("quota.unavailable")} />
        )}
      </ConfigBlock>

      {/* ---- reset cards ---- */}
      <ConfigBlock title={t("zconfig.resetCards")} divided>
        {/* The hub lists the provider ids that COULD own cards; it does not
            know which one this account actually has, so the user picks. Team
            plans are offered too — they are selectable and then explained,
            because hiding them would look like the plan is missing. */}
        {eligible.length > 1 && (
          <div className="flex flex-wrap gap-2 px-4 pb-2">
            {eligible.map((id) => (
              <button
                key={id}
                disabled={resetBusy}
                onClick={() => void loadResetCards(id)}
                className={`rounded-lg px-2.5 py-1.5 text-2xs transition disabled:opacity-50 ${
                  id === providerId
                    ? "bg-active font-medium text-ink"
                    : "text-faint hover:bg-hover active:bg-active"
                }`}
              >
                {providerLabel(id, t)}
              </button>
            ))}
          </div>
        )}

        {!credentialsOk ? (
          // The credential store is encrypted per machine; not being able to
          // read it is a normal state, and it is distinct from having no plan.
          <ConfigEmpty text={t("zconfig.resetUnavailable")} />
        ) : !providerId ? (
          <ConfigEmpty text={t("zconfig.resetPickProvider")} />
        ) : teamBlocked ? (
          // Not a missing feature — the request cannot work from a headless
          // process, so offer the desktop app instead of a dead button.
          <ConfigEmpty text={t("zconfig.resetTeamBlocked")} />
        ) : resetBusy && !resetCards ? (
          <ConfigEmpty text={t("zconfig.resetLoading")} />
        ) : resetCards ? (
          <>
            <CardList
              title={t("zconfig.resetFiveHour")}
              cards={resetCards.availableFiveHour}
              lastUsed={resetCards.latestFiveHour?.usedAt ?? null}
              resetType="FIVE_HOUR"
              aboveThreshold={
                windowUsedPercent(usageStats, "FIVE_HOUR") >=
                SPEND_THRESHOLD_PCT
              }
              busy={resetBusy}
              nextTryAt={resetNextTryAt}
              confirm={confirm}
              onConfirm={setConfirm}
              onAsk={askSpend}
              onSpend={spend}
            />
            <CardList
              title={t("zconfig.resetWeek")}
              cards={resetCards.availableWeek}
              lastUsed={resetCards.latestWeek?.usedAt ?? null}
              resetType="WEEK"
              aboveThreshold={
                windowUsedPercent(usageStats, "WEEK") >= SPEND_THRESHOLD_PCT
              }
              busy={resetBusy}
              nextTryAt={resetNextTryAt}
              confirm={confirm}
              onConfirm={setConfirm}
              onAsk={askSpend}
              onSpend={spend}
            />
          </>
        ) : (
          <ConfigEmpty text={t("zconfig.resetUnavailable")} />
        )}
      </ConfigBlock>

      {usageStatsAt != null && (
        <p className="px-4 pt-3 text-center text-2xs text-faint">
          {t("quota.updated")} {fmtStamp(usageStatsAt)}
        </p>
      )}
    </ConfigPageFrame>
  );
}

/**
 * One window's card inventory: how many are left, when each expires, and the
 * spend control.
 *
 * A card carries an expiry and nothing else — the API exposes no card number
 * or denomination — so this is all there is to show. Soonest-expiring first,
 * because that is the order they should be spent in.
 */
function CardList({
  title,
  cards,
  lastUsed,
  resetType,
  aboveThreshold,
  busy,
  nextTryAt,
  confirm,
  onConfirm,
  onAsk,
  onSpend,
}: {
  title: string;
  cards: Array<{ expireAt: number }>;
  lastUsed: number | null;
  resetType: ResetType;
  aboveThreshold: boolean;
  busy: boolean;
  nextTryAt: number | null;
  confirm: SpendStage | null;
  onConfirm: (v: SpendStage | null) => void;
  onAsk: (type: ResetType) => void;
  onSpend: (v: ResetType) => void;
}) {
  const { t } = useTranslation();
  const sorted = [...cards].sort((a, b) => a.expireAt - b.expireAt);
  const next = sorted[0];

  return (
    <div className="px-4 py-3">
      <div className="flex items-baseline justify-between gap-3">
        <span className="text-sm text-dim">{title}</span>
        <span className="text-sm tabular-nums text-ink">{cards.length}</span>
      </div>

      {cards.length === 0 ? (
        <p className="pt-1 text-xs text-faint">{t("zconfig.resetNone")}</p>
      ) : (
        <ul className="pt-1">
          {sorted.map((c, i) => (
            <li
              key={`${c.expireAt}-${i}`}
              className="flex items-center gap-2 py-0.5 text-xs text-faint"
            >
              <Check className="size-3 shrink-0 text-success/70" />
              <span className="tabular-nums">
                {t("zconfig.resetExpires")} {fmtStamp(c.expireAt)}
              </span>
              {i === 0 && (
                <span className="ml-auto shrink-0 text-2xs text-faint">
                  {t("zconfig.resetNextUp")}
                </span>
              )}
            </li>
          ))}
        </ul>
      )}

      {lastUsed != null && (
        <p className="pt-1 text-2xs text-faint">
          {t("zconfig.resetLastUsed")} {fmtStamp(lastUsed)}
        </p>
      )}

      {nextTryAt != null && nextTryAt > Date.now() && (
        <p className="pt-1 text-2xs text-warning">
          {t("zconfig.resetRetryAt")} {fmtStamp(nextTryAt)}
        </p>
      )}

      {cards.length > 0 &&
        (confirm?.type === resetType ? (
          confirm.warned ? (
            // The spend confirmation names the card, not a generic "are you
            // sure": a spend is irreversible and the user is committing to a
            // specific window and expiry.
            <div className="mt-2 rounded-lg bg-hover px-3 py-2.5">
              <p className="text-xs text-dim">
                {t("zconfig.resetConfirm", {
                  title: title.toLowerCase(),
                  expires: next ? fmtStamp(next.expireAt) : "",
                })}
              </p>
              <div className="mt-2 flex gap-2">
                <button
                  onClick={() => onConfirm(null)}
                  className="flex-1 rounded-lg px-3 py-2 text-xs text-faint hover:bg-hover active:bg-active"
                >
                  {t("common.cancel")}
                </button>
                <button
                  disabled={busy}
                  onClick={() => void onSpend(resetType)}
                  className="flex-1 rounded-lg bg-active px-3 py-2 text-xs font-medium text-ink active:bg-active disabled:opacity-60"
                >
                  {busy ? (
                    <RefreshCw className="mx-auto size-3.5 animate-spin" />
                  ) : (
                    t("zconfig.resetSpend")
                  )}
                </button>
              </div>
            </div>
          ) : (
            // The pre-step below the threshold: the card works, but the
            // allowance is nowhere near spent — say so before the user
            // commits to anything.
            <div className="mt-2 rounded-lg bg-warning-wash px-3 py-2.5">
              <p className="text-xs text-warning">
                {t("zconfig.resetBelowWarn", { pct: SPEND_THRESHOLD_PCT })}
              </p>
              <div className="mt-2 flex gap-2">
                <button
                  onClick={() => onConfirm(null)}
                  className="flex-1 rounded-lg px-3 py-2 text-xs text-faint hover:bg-hover active:bg-active"
                >
                  {t("common.cancel")}
                </button>
                <button
                  onClick={() => onConfirm({ type: resetType, warned: true })}
                  className="flex-1 rounded-lg bg-warning-wash px-3 py-2 text-xs font-medium text-warning active:brightness-125"
                >
                  {t("zconfig.resetWarnContinue")}
                </button>
              </div>
            </div>
          )
        ) : (
          <button
            disabled={busy}
            onClick={() => onAsk(resetType)}
            className="mt-2 w-full rounded-lg bg-selected px-3 py-2 text-xs font-medium text-ink hover:bg-hover active:bg-active disabled:opacity-40"
          >
            {t("zconfig.resetUse")}
          </button>
        ))}

      {/* Below this card's OWN threshold the card still works — the note says
          what the extra step is, not that the button is dead. */}
      {!aboveThreshold && (
        <p className="pt-1.5 text-2xs text-faint">
          {t("zconfig.resetBelowThreshold", { pct: SPEND_THRESHOLD_PCT })}
        </p>
      )}
    </div>
  );
}
