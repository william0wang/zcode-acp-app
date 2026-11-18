import type { ButtonHTMLAttributes, ReactNode } from "react";
import { cn } from "./utils";

/*
 * The one button of the app. Variants cover the three intents used across
 * screens; sizes are the vertical rhythm (h-8 controls rows, h-10 primary
 * actions). All interactive states (hover / active / focus-visible /
 * disabled) are built in — call sites never add their own.
 *
 * Sanctioned literals: `text-white` on the danger fill and `text-canvas` on
 * the ink fill are the two allowed non-token text colors (contrast pairs).
 */

type Variant = "primary" | "secondary" | "ghost" | "danger";
type Size = "sm" | "md";

const base =
  "inline-flex select-none items-center justify-center gap-1.5 rounded-md font-medium " +
  "outline-none transition-colors duration-100 " +
  "focus-visible:ring-2 focus-visible:ring-ink/40 " +
  "disabled:pointer-events-none disabled:opacity-50";

const variants: Record<Variant, string> = {
  primary: "bg-ink text-canvas active:brightness-90",
  secondary:
    "bg-raised text-ink ring-1 ring-hairline hover:bg-hover active:bg-active",
  ghost: "bg-transparent text-dim hover:bg-hover hover:text-ink active:bg-active",
  danger:
    "bg-danger-strong text-white outline-danger-strong/60 hover:brightness-110 active:brightness-90",
};

const sizes: Record<Size, string> = {
  sm: "h-8 px-3 text-xs",
  md: "h-10 px-4 text-sm",
};

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant;
  size?: Size;
  /** Swaps the label for a spinner and blocks interaction. */
  loading?: boolean;
  leading?: ReactNode;
}

export function Button({
  variant = "secondary",
  size = "md",
  loading = false,
  leading,
  className,
  children,
  disabled,
  type = "button",
  ...rest
}: ButtonProps) {
  return (
    <button
      type={type}
      className={cn(base, variants[variant], sizes[size], className)}
      disabled={disabled || loading}
      aria-busy={loading || undefined}
      {...rest}
    >
      {loading ? (
        <span
          aria-hidden
          className="size-3.5 animate-spin rounded-full border border-current border-t-transparent"
        />
      ) : (
        leading
      )}
      {children}
    </button>
  );
}
