import type { ComponentProps, ReactNode } from "react";
import { cn } from "./utils";

/*
 * Text input with optional label / hint / error. The raised fill + hairline
 * ring matches the composer shell; focus swaps to an ink ring so keyboard
 * navigation is visible on the dark palette.
 */

export interface InputProps extends Omit<ComponentProps<"input">, "className"> {
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  /** Renders a monospace face (paths, tokens, commands). */
  mono?: boolean;
  className?: string;
  inputClassName?: string;
}

export function Input({
  label,
  hint,
  error,
  mono = false,
  className,
  inputClassName,
  id,
  ...rest
}: InputProps) {
  const inputId = id ?? (label ? String(label).replace(/\s+/g, "-").toLowerCase() : undefined);
  return (
    <label htmlFor={inputId} className={cn("block", className)}>
      {label ? (
        <span className="mb-1.5 block text-xs text-dim">{label}</span>
      ) : null}
      <input
        id={inputId}
        aria-invalid={error ? true : undefined}
        className={cn(
          "h-10 w-full rounded-md bg-raised px-3 text-sm text-ink",
          "placeholder:text-faint outline-none transition-colors",
          "ring-1 focus-visible:ring-2 focus-visible:ring-ink/50",
          mono && "font-mono",
          error ? "ring-danger" : "ring-hairline",
          inputClassName,
        )}
        {...rest}
      />
      {error ? (
        <span className="mt-1.5 block text-2xs text-danger">{error}</span>
      ) : hint ? (
        <span className="mt-1.5 block text-2xs text-faint">{hint}</span>
      ) : null}
    </label>
  );
}
