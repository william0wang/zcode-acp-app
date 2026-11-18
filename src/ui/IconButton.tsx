import type { ButtonHTMLAttributes, ReactNode } from "react";
import { cn } from "./utils";

/*
 * Square icon-only control. `label` is mandatory — an icon button without
 * an accessible name is the most common a11y hole this layer exists to
 * close. When `tip` is given the same string doubles as a hover tooltip
 * (pointer users) — the aria-label always stands on its own.
 */

type Variant = "ghost" | "raised" | "danger";
type Size = "sm" | "md";

const base =
  "inline-flex select-none items-center justify-center rounded-full outline-none " +
  "transition-colors duration-100 " +
  "focus-visible:ring-2 focus-visible:ring-ink/40 " +
  "disabled:pointer-events-none disabled:opacity-50";

const variants: Record<Variant, string> = {
  ghost: "bg-transparent text-dim hover:bg-hover hover:text-ink active:bg-active",
  raised:
    "bg-raised text-ink ring-1 ring-hairline hover:bg-hover active:bg-active",
  danger: "bg-danger-wash text-danger hover:brightness-125 active:brightness-90",
};

const sizes: Record<Size, string> = {
  sm: "size-7 p-1.5",
  md: "size-9 p-2",
};

export interface IconButtonProps
  extends ButtonHTMLAttributes<HTMLButtonElement> {
  /** Accessible name; also the tooltip when `tip` is not given. */
  label: string;
  /** Optional tooltip text differing from the accessible name. */
  tip?: string;
  variant?: Variant;
  size?: Size;
  /** Small status pip attached to the top-right corner. */
  badge?: ReactNode;
}

export function IconButton({
  label,
  tip,
  variant = "ghost",
  size = "md",
  badge,
  className,
  children,
  type = "button",
  ...rest
}: IconButtonProps) {
  return (
    <span className={cn("relative inline-flex", className)}>
      <button
        type={type}
        aria-label={label}
        title={tip ?? label}
        className={cn(base, variants[variant], sizes[size])}
        {...rest}
      >
        {children}
      </button>
      {badge ? (
        <span className="pointer-events-none absolute -right-0.5 -top-0.5">
          {badge}
        </span>
      ) : null}
    </span>
  );
}
