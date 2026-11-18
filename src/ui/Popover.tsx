import type { ReactElement, ReactNode } from "react";
import { Popover as BasePopover } from "@base-ui/react/popover";
import { cn } from "./utils";

/*
 * Anchored popover: menus, pickers, the model/mode pill panel. The trigger
 * is a single element (Base UI merges its props — open state lands as a
 * data attribute, so triggers can style themselves with
 * `data-[popup-open]:…`). Menu rows that should close on click wrap
 * themselves in the exported PopoverClose.
 */

export interface PopoverProps {
  trigger: ReactElement;
  children: ReactNode;
  align?: "start" | "center" | "end";
  className?: string;
}

export function Popover({
  trigger,
  children,
  align = "start",
  className,
}: PopoverProps) {
  return (
    <BasePopover.Root>
      <BasePopover.Trigger>{trigger}</BasePopover.Trigger>
      <BasePopover.Portal>
        <BasePopover.Positioner
          side="bottom"
          align={align}
          sideOffset={6}
          className="z-50 outline-none"
        >
          <BasePopover.Popup
            className={cn(
              "animate-fade-in min-w-40 rounded-lg bg-surface p-1.5 ring-1 ring-hairline shadow-pop",
              className,
            )}
          >
            {children}
          </BasePopover.Popup>
        </BasePopover.Positioner>
      </BasePopover.Portal>
    </BasePopover.Root>
  );
}

/** Wrap a menu row to close the popover on click. */
export const PopoverClose = BasePopover.Close;
