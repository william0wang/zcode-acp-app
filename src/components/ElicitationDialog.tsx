import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { ListChecks } from "lucide-react";
import { Sheet } from "../ui";
import { haptic } from "../lib/haptic";
import { useAppStore, type ElicitField } from "../store/appStore";

// elicitation/create forms — the bridge's preferred channel once any client
// advertises elicitation.form (we declare it in initialize). Two shapes:
// AskUserQuestion question sets (one bottom sheet per field set; unanswered
// fields are skipped, Cancel declines the whole form) and the plan-approval
// form (bridge 0.33+), a single approve/reject field whose `description`
// carries the COMPLETE plan markdown — rendered as a review card with the
// plan in a scrollable pane, not a blind approve/reject.

// Plan-approval field: single-select approve/reject (the values the bridge's
// buildPlanApprovalElicitationForm hardcodes; titles are backend-localized).
function isPlanField(f: ElicitField): boolean {
  return (
    !f.multi &&
    f.otherKey == null &&
    f.options.some((o) => o.value === "approve") &&
    f.options.some((o) => o.value === "reject")
  );
}

function QuestionBlock({
  field,
  picked,
  checked,
  other,
  onPick,
  onToggle,
  onOther,
}: {
  field: ElicitField;
  picked: string | null;
  checked: Record<string, boolean>;
  other: string;
  onPick: (v: string) => void;
  onToggle: (v: string) => void;
  onOther: (v: string) => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="px-4 pb-3 pt-1">
      <p className="pb-1.5 text-sm text-ink">{field.question}</p>
      {field.description && (
        <div className="prose prose-sm prose-invert max-w-none pb-2">
          <Markdown remarkPlugins={[remarkGfm]}>{field.description}</Markdown>
        </div>
      )}
      <div className="flex flex-col gap-1.5">
        {field.options.map((o) => {
          const on = field.multi ? checked[o.value] : picked === o.value;
          return (
            <button
              key={o.value}
              onClick={() =>
                field.multi ? onToggle(o.value) : onPick(o.value)
              }
              className={`rounded-xl px-3.5 py-2.5 text-left text-sm transition ${
                on
                  ? "bg-ink text-canvas active:brightness-90"
                  : "bg-raised text-dim ring-1 ring-inset ring-hairline hover:bg-hover active:bg-active"
              }`}
            >
              {o.label}
            </button>
          );
        })}
        {field.otherKey != null && (
          <input
            value={other}
            onChange={(e) => onOther(e.target.value)}
            placeholder={
              field.multi ? t("permission.otherValueMulti") : t("permission.otherValue")
            }
            autoCapitalize="none"
            className="rounded-xl bg-raised px-3.5 py-2.5 text-sm text-ink placeholder:text-faint ring-1 ring-inset ring-hairline focus:outline-none"
          />
        )}
      </div>
    </div>
  );
}

export function ElicitationDialog() {
  const { t } = useTranslation();
  // Per-session rendering (bridge 0.17.0 semantics): the form shows ONLY in
  // the session that asked — see PermissionDialog for the rationale.
  const elicitation = useAppStore(
    (s) => (s.activeSessionId ? s.elicitations[s.activeSessionId] : undefined) ?? null,
  );
  const answerElicitation = useAppStore((s) => s.answerElicitation);
  // Approval queue count (requirements 0.3) — same card as PermissionDialog.
  const queueCount = useAppStore((s) => {
    let n = 0;
    for (const sid of Object.keys(s.permissions))
      if (sid !== s.activeSessionId) n++;
    for (const sid of Object.keys(s.elicitations))
      if (sid !== s.activeSessionId) n++;
    return n;
  });
  // Local answer state; keyed fresh per request (option sets can repeat q_0).
  const [picked, setPicked] = useState<Record<string, string>>({});
  const [checked, setChecked] = useState<Record<string, boolean>>({});
  const [other, setOther] = useState<Record<string, string>>({});

  const requestId = elicitation?.requestId;
  useEffect(() => {
    setPicked({});
    setChecked({});
    setOther({});
  }, [requestId]);

  // Plan-approval form (exactly one approve/reject field): direct-answer
  // buttons instead of the pick-then-submit flow — same card as the
  // request_permission plan fallback (PermissionDialog).
  const planField =
    (elicitation?.fields.length ?? 0) === 1 && elicitation
      ? isPlanField(elicitation.fields[0])
        ? elicitation.fields[0]
        : null
      : null;

  // Keyboard answers (requirements 0.3): Enter confirms (approve on the plan
  // form, submit on the question form — including from the custom-value
  // input), Escape declines the whole form, mirroring the Cancel button.
  // IME-confirm Enters are skipped (isComposing).
  useEffect(() => {
    if (!elicitation) return;
    const req = elicitation;
    const onKey = (e: KeyboardEvent) => {
      if (e.isComposing || e.metaKey || e.ctrlKey || e.altKey) return;
      if (e.key === "Enter") {
        haptic();
        if (planField) {
          answerElicitation(req.requestId, { [planField.key]: "approve" });
        } else {
          const content: Record<string, string | string[]> = {};
          for (const f of req.fields) {
            const free =
              f.otherKey != null ? (other[f.otherKey] ?? "").trim() : "";
            if (f.multi) {
              const sel = f.options
                .filter((o) => checked[o.value])
                .map((o) => o.value);
              if (sel.length > 0) content[f.key] = sel;
            } else if (!free) {
              const v = picked[f.key];
              if (v) content[f.key] = v;
            }
            if (free && f.otherKey != null) content[f.otherKey] = free;
          }
          answerElicitation(req.requestId, content);
        }
      } else if (e.key === "Escape") {
        haptic();
        answerElicitation(req.requestId, null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [elicitation, planField, picked, checked, other, answerElicitation]);

  if (!elicitation) return null;

  if (planField) {
    const approve = planField.options.find((o) => o.value === "approve");
    const reject = planField.options.find((o) => o.value === "reject");
    const answer = (v: string) => {
      haptic();
      answerElicitation(elicitation.requestId, { [planField.key]: v });
    };
    return (
      <Sheet
        open
        blocking
        onClose={() => {}}
        className="flex max-h-[75vh] w-full flex-col overflow-hidden"
      >
        <div className="-mx-4 min-h-0 flex-1 overflow-y-auto px-4">
          <h2 className="flex items-center gap-1.5 text-sm font-semibold text-ink">
            <ListChecks className="size-4 shrink-0 text-info" />
            {elicitation.message || t("permission.planTitle")}
          </h2>
          {planField.description && (
            <div className="prose prose-sm prose-invert mt-2 max-w-none">
              <Markdown remarkPlugins={[remarkGfm]}>
                {planField.description}
              </Markdown>
            </div>
          )}
        </div>
        <div className="pb-1 pt-3">
          <button
            onClick={() => answer("approve")}
            className="w-full rounded-xl bg-success px-4 py-3 text-sm font-semibold text-canvas active:brightness-90"
          >
            {approve?.label || t("permission.approve")}
          </button>
          <button
            onClick={() => answer("reject")}
            className="mt-2 w-full rounded-xl bg-danger-wash px-4 py-3 text-sm font-medium text-danger ring-1 ring-inset ring-danger/40 active:brightness-125"
          >
            {reject?.label || t("permission.reject")}
          </button>
          <button
            onClick={() => {
              haptic();
              answerElicitation(elicitation.requestId, null);
            }}
            className="mt-2 w-full rounded-xl bg-raised px-4 py-3 text-sm font-medium text-ink ring-1 ring-inset ring-hairline hover:bg-hover active:bg-active"
          >
            {t("common.cancel")}
          </button>
          {queueCount > 0 && (
            <p className="mt-2 text-center text-xs font-medium text-warning">
              {t("permission.queueWaiting", { count: queueCount })}
            </p>
          )}
        </div>
      </Sheet>
    );
  }

  const submit = () => {
    const content: Record<string, string | string[]> = {};
    for (const f of elicitation.fields) {
      const free = f.otherKey != null ? (other[f.otherKey] ?? "").trim() : "";
      if (f.multi) {
        const sel = f.options
          .filter((o) => checked[o.value])
          .map((o) => o.value);
        if (sel.length > 0) content[f.key] = sel;
      } else if (!free) {
        // No custom text: the plain pick decides; no pick = skipped question.
        const v = picked[f.key];
        if (v) content[f.key] = v;
      }
      if (free && f.otherKey != null) content[f.otherKey] = free;
    }
    haptic();
    answerElicitation(elicitation.requestId, content);
  };

  return (
    <Sheet
      open
      blocking
      onClose={() => {}}
      className="flex max-h-[75vh] w-full flex-col overflow-hidden"
    >
      <div className="-mx-4 min-h-0 flex-1 overflow-y-auto">
        <h2 className="px-4 pt-1 text-sm font-semibold text-ink">
          {elicitation.message || t("permission.questionTitle")}
        </h2>
        {elicitation.fields.map((f) => (
          <QuestionBlock
            key={f.key}
            field={f}
            picked={picked[f.key] ?? null}
            checked={checked}
            other={f.otherKey != null ? (other[f.otherKey] ?? "") : ""}
            onPick={(v) => setPicked((p) => ({ ...p, [f.key]: v }))}
            onToggle={(v) => setChecked((c) => ({ ...c, [v]: !c[v] }))}
            onOther={(v) => setOther((o) => ({ ...o, [f.otherKey!]: v }))}
          />
        ))}
      </div>
      <div className="pb-1 pt-1">
        <button
          onClick={submit}
          className="w-full rounded-xl bg-ink px-4 py-3 text-sm font-semibold text-canvas active:brightness-90"
        >
          {t("permission.submit")}
        </button>
        <button
          onClick={() => {
            haptic();
            answerElicitation(elicitation.requestId, null);
          }}
          className="mt-2 w-full rounded-xl bg-raised px-4 py-3 text-sm font-medium text-ink ring-1 ring-inset ring-hairline hover:bg-hover active:bg-active"
        >
          {t("common.cancel")}
        </button>
        {queueCount > 0 && (
          <p className="mt-2 text-center text-xs font-medium text-warning">
            {t("permission.queueWaiting", { count: queueCount })}
          </p>
        )}
      </div>
    </Sheet>
  );
}
