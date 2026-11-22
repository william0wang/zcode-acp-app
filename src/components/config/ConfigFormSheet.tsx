import { useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { ArrowLeft, Trash2 } from "lucide-react";
import { useBackHandler } from "../../lib/backNav";
import { useAppStore } from "../../store/appStore";

// Input styling shared by every configuration form — the same recipe the
// connect screen uses, so a keyboard-focus ring reads the same everywhere.
export const configInputClass =
  "mt-1 w-full rounded-xl bg-raised px-4 py-3 text-sm text-ink ring-1 ring-inset ring-hairline placeholder:text-faint focus:ring-2 focus:ring-ink/40 focus:outline-none";

// A labelled form field. The label wraps the control so a tap on the text
// focuses the input, the way the connect form behaves.
export function ConfigField({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: ReactNode;
}) {
  return (
    <label className="mt-4 block">
      <span className="block text-xs font-medium text-dim">{label}</span>
      {children}
      {hint && (
        <span className="mt-1 block text-2xs text-faint">{hint}</span>
      )}
    </label>
  );
}

/**
 * Full-screen form overlay for the configuration pages (ADR-0009): editing a
 * model rule, an MCP server, or a subagent replaces the section page outright
 * — the same "a full screen of its own" rule the section pages follow — so a
 * half-open form never sits behind the section list.
 *
 * Form contract (ADR-0014): Save is the caller's `submitDisabled` (valid AND
 * dirty), a dirty close discards with a toast, and `footerLeft` is the edit
 * form's Delete slot — delete lives in the edit form's footer, never on rows.
 *
 * Saving routes through `applyConfigWrite` at the call site, which toasts the
 * outcome; the sheet just closes when the caller reports success.
 */
export function ConfigFormSheet({
  title,
  busy,
  submitDisabled,
  dirty,
  footerLeft,
  onSubmit,
  onClose,
  children,
}: {
  title: string;
  busy?: boolean;
  submitDisabled?: boolean;
  /** A dirty close discards the draft — say so (the mobile form contract). */
  dirty?: boolean;
  /** The edit form's delete control (rendered LEFT of Save). */
  footerLeft?: ReactNode;
  onSubmit: () => void;
  onClose: () => void;
  children: ReactNode;
}) {
  const { t } = useTranslation();
  // Back out of the form the same way the header arrow does; the gesture
  // never submits. A dirty draft discards with a toast so the leave is never
  // silent.
  const close = () => {
    if (dirty) useAppStore.getState().notify(t("zconfig.discarded"));
    onClose();
  };
  useBackHandler(() => {
    close();
    return true;
  });
  const save = (
    <button
      onClick={onSubmit}
      disabled={busy || submitDisabled}
      className="rounded-xl bg-white px-5 py-3 text-sm font-semibold text-black transition active:scale-[0.99] disabled:opacity-40"
    >
      {busy ? t("zconfig.saving") : t("common.save")}
    </button>
  );
  return (
    <div className="fixed inset-0 z-50 flex flex-col bg-canvas text-ink">
      <header className="flex shrink-0 items-center gap-1 px-3 pb-2 pt-[max(var(--safe-top),0.75rem)]">
        <button
          onClick={close}
          aria-label={t("common.close")}
          className="flex size-9 shrink-0 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active"
        >
          <ArrowLeft className="size-4.5" />
        </button>
        <h1 className="min-w-0 flex-1 truncate text-base font-semibold">
          {title}
        </h1>
      </header>
      <div className="flex-1 overflow-y-auto px-4 pb-[max(var(--safe-bottom),1rem)]">
        {children}
        {footerLeft ? (
          <div className="mt-6 flex items-center gap-3">
            <div className="min-w-0 shrink">{footerLeft}</div>
            <div className="min-w-0 flex-1" />
            {save}
          </div>
        ) : (
          <div className="mt-6">{save}</div>
        )}
      </div>
    </div>
  );
}

/**
 * The edit form's two-tap delete (the form contract's footer control): the
 * first tap arms it, the second fires `onConfirm`. Destructive and undoless,
 * so one stray tap must never do it.
 */
export function FormDeleteButton({
  label,
  busy,
  onConfirm,
}: {
  label: string;
  busy?: boolean;
  onConfirm: () => void;
}) {
  const { t } = useTranslation();
  const [armed, setArmed] = useState(false);
  if (armed) {
    return (
      <span className="flex items-center gap-2">
        <button
          onClick={() => setArmed(false)}
          className="rounded-lg px-2 py-2 text-xs text-dim hover:bg-hover active:bg-active"
        >
          {t("common.cancel")}
        </button>
        <button
          onClick={onConfirm}
          disabled={busy}
          className="rounded-lg bg-danger-wash px-3 py-2 text-xs font-medium text-danger ring-1 ring-inset ring-danger/40 active:brightness-125 disabled:opacity-50"
        >
          {label}
        </button>
      </span>
    );
  }
  return (
    <button
      onClick={() => setArmed(true)}
      aria-label={label}
      className="flex size-10 items-center justify-center rounded-xl text-danger/80 hover:bg-hover active:bg-active"
    >
      <Trash2 className="size-4.5" />
    </button>
  );
}

/**
 * The same toggle switch the section rows use, for use inside a form.
 * Controlled: the form owns the boolean.
 */
export function ConfigToggle({
  checked,
  label,
  onChange,
}: {
  checked: boolean;
  label: string;
  onChange: (next: boolean) => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      onClick={() => onChange(!checked)}
      className={`mt-1 h-6 w-10 shrink-0 rounded-full transition ${
        checked ? "bg-success/80" : "bg-active"
      }`}
    >
      <span
        className={`block size-5 rounded-full bg-white transition ${
          checked ? "translate-x-4.5" : "translate-x-0.5"
        }`}
      />
    </button>
  );
}

/**
 * Split a KEY=VALUE textarea into a record. Lines without a separator are
 * skipped rather than erroring — the user is mid-typing.
 */
export function parseKvLines(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    const eq = trimmed.indexOf("=");
    if (eq <= 0) continue;
    out[trimmed.slice(0, eq).trim()] = trimmed.slice(eq + 1);
  }
  return out;
}

/** The inverse of `parseKvLines`, for prefilling a textarea from a record. */
export function kvToLines(record: Record<string, string> | undefined): string {
  return Object.entries(record ?? {})
    .map(([k, v]) => `${k}=${v}`)
    .join("\n");
}

/** Non-blank lines of a one-per-line textarea. */
export function splitLines(text: string): string[] {
  return text
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l !== "");
}
