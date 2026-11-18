import type { HTMLAttributes } from "react";
import { cn } from "./utils";

/*
 * Count pip and status dot. Counts clamp at 99+. Status variants pair a
 * wash fill with the value color — the same pair tokens banners use.
 */

type Tone = "neutral" | "success" | "warning" | "danger" | "info";

const tones: Record<Tone, string> = {
  neutral: "bg-raised text-dim ring-hairline",
  success: "bg-success-wash text-success ring-success/30",
  warning: "bg-warning-wash text-warning ring-warning/30",
  danger: "bg-danger-wash text-danger ring-danger/30",
  info: "bg-info-wash text-info ring-info/30",
};

export interface BadgeProps extends HTMLAttributes<HTMLSpanElement> {
  tone?: Tone;
  /** Numeric badge; ≥100 renders "99+". */
  count?: number;
  /** Solid dot instead of a number. */
  dot?: boolean;
}

export function Badge({
  tone = "neutral",
  count,
  dot = false,
  className,
  children,
  ...rest
}: BadgeProps) {
  if (dot) {
    return (
      <span
        aria-hidden
        className={cn("inline-block size-1.5 rounded-full", tones[tone], className)}
        {...rest}
      />
    );
  }
  const text =
    count !== undefined ? (count > 99 ? "99+" : String(count)) : children;
  return (
    <span
      className={cn(
        "inline-flex min-w-4 items-center justify-center rounded-full px-1 text-2xs font-medium ring-1",
        tones[tone],
        className,
      )}
      {...rest}
    >
      {text}
    </span>
  );
}
