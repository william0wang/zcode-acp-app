import type { HTMLAttributes, ReactNode } from "react";
import { cn } from "./utils";

/*
 * Presentational toast: role="status" so screen readers announce it without
 * stealing focus. Tone pairs a status accent with the neutral surface; the
 * app's toast store (appStore.notifyToast) owns timing and dedup — this is
 * only the look.
 */

type Tone = "neutral" | "success" | "warning" | "danger" | "info";

const bars: Record<Tone, string> = {
  neutral: "bg-hairline",
  success: "bg-success",
  warning: "bg-warning",
  danger: "bg-danger",
  info: "bg-info",
};

export interface ToastProps extends HTMLAttributes<HTMLDivElement> {
  tone?: Tone;
  /** Right-aligned dismissal / action node. */
  action?: ReactNode;
}

export function Toast({
  tone = "neutral",
  action,
  className,
  children,
  ...rest
}: ToastProps) {
  return (
    <div
      role="status"
      className={cn(
        "animate-slide-up flex items-center gap-3 overflow-hidden rounded-lg",
        "bg-raised py-2.5 pl-4 pr-3 text-sm text-ink ring-1 ring-hairline shadow-pop",
        className,
      )}
      {...rest}
    >
      <span aria-hidden className={cn("h-4 w-0.5 shrink-0 rounded-full", bars[tone])} />
      <div className="min-w-0 flex-1">{children}</div>
      {action ? <div className="shrink-0">{action}</div> : null}
    </div>
  );
}
