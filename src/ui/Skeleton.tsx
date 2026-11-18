import type { HTMLAttributes } from "react";
import { cn } from "./utils";

/*
 * Loading placeholder. The pulse is the battery-quantized one from the
 * theme (steps(), not a compositor-churning smooth fade).
 */

export interface SkeletonProps extends HTMLAttributes<HTMLDivElement> {
  /** Shape shorthand; pass className for anything else. */
  shape?: "line" | "block" | "circle";
}

export function Skeleton({
  shape = "line",
  className,
  ...rest
}: SkeletonProps) {
  return (
    <div
      aria-hidden
      className={cn(
        "animate-pulse rounded-md bg-hover",
        shape === "line" && "h-3.5 w-full",
        shape === "block" && "h-20 w-full",
        shape === "circle" && "size-9 rounded-full",
        className,
      )}
      {...rest}
    />
  );
}
