import type { ReactNode } from "react";
import { Dialog as BaseDialog } from "@base-ui/react/dialog";
import { cn } from "./utils";

/*
 * Bottom sheet (the mobile idiom): docks to the viewport bottom above the
 * safe area, slides up, rounded top corners. Desktop side panels keep using
 * the domain Drawer — this is the modal bottom layer only.
 */

export interface SheetProps {
  open: boolean;
  onClose: () => void;
  title?: ReactNode;
  children?: ReactNode;
  className?: string;
}

export function Sheet({
  open,
  onClose,
  title,
  children,
  className,
}: SheetProps) {
  return (
    <BaseDialog.Root
      open={open}
      onOpenChange={(next) => {
        if (!next) onClose();
      }}
    >
      <BaseDialog.Portal>
        <BaseDialog.Backdrop className="fixed inset-0 z-50 bg-canvas/70 animate-fade-in" />
        <BaseDialog.Popup
          className={cn(
            "fixed inset-x-0 bottom-0 z-50 mx-auto max-w-2xl",
            "animate-sheet-in rounded-t-2xl bg-surface ring-1 ring-hairline shadow-overlay",
            "px-4 pb-[calc(1rem+var(--safe-bottom))] pt-3 outline-none",
            className,
          )}
        >
          {/* Grabber: the affordance that this layer slides. */}
          <div aria-hidden className="mx-auto mb-3 h-1 w-10 rounded-full bg-hairline" />
          {title ? (
            <BaseDialog.Title className="mb-3 text-sm font-semibold text-ink">
              {title}
            </BaseDialog.Title>
          ) : null}
          {children}
        </BaseDialog.Popup>
      </BaseDialog.Portal>
    </BaseDialog.Root>
  );
}
