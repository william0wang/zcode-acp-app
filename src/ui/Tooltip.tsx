import type { ReactElement } from "react";
import { Tooltip as BaseTooltip } from "@base-ui/react/tooltip";

/*
 * Hover/focus tooltip for pointer+keyboard users. Wraps a single element
 * child (Base UI's Trigger merges its props onto the child). Labels stay in
 * aria-label/`label` props — the tooltip text is supplementary.
 */

export interface TooltipProps {
  tip: string;
  children: ReactElement;
  /** Open delay in ms (default 350 keeps accidental brushes away). */
  delay?: number;
}

export function Tooltip({ tip, children, delay = 350 }: TooltipProps) {
  return (
    <BaseTooltip.Provider delay={delay}>
      <BaseTooltip.Root>
        <BaseTooltip.Trigger>{children}</BaseTooltip.Trigger>
        <BaseTooltip.Portal>
          <BaseTooltip.Positioner sideOffset={6}>
            <BaseTooltip.Popup className="animate-fade-in rounded-md bg-raised px-2 py-1 text-2xs text-ink ring-1 ring-hairline shadow-pop">
              {tip}
            </BaseTooltip.Popup>
          </BaseTooltip.Positioner>
        </BaseTooltip.Portal>
      </BaseTooltip.Root>
    </BaseTooltip.Provider>
  );
}
