import type { HTMLAttributes } from "react";
import { cn } from "./utils";

/*
 * The one panel container: surface fill, hairline ring, xl radius. The
 * `interactive` variant adds the hover/active overlay pair and a cursor —
 * for list rows and tappable cards.
 */

export interface CardProps extends HTMLAttributes<HTMLDivElement> {
  interactive?: boolean;
  /** Extra inset override; default p-4. */
  padded?: boolean;
}

export function Card({
  interactive = false,
  padded = true,
  className,
  ...rest
}: CardProps) {
  return (
    <div
      className={cn(
        "rounded-xl bg-surface ring-1 ring-hairline",
        padded && "p-4",
        interactive &&
          "cursor-pointer outline-none transition-colors hover:bg-hover active:bg-active focus-visible:ring-2 focus-visible:ring-ink/40",
        className,
      )}
      {...rest}
    />
  );
}
