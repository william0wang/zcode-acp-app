import type { ReactNode } from "react";
import { Dialog as BaseDialog } from "@base-ui/react/dialog";
import { cn } from "./utils";

/*
 * Centered modal: confirmations, forms, approval dialogs. `blocking` is the
 * permission-dialog mode — neither escape nor backdrop closes it; only the
 * explicit action buttons (footer) resolve the ask, so an agent question can
 * never be dismissed by accident (see the approval-UX requirements).
 */

export interface DialogProps {
  open: boolean;
  onClose: () => void;
  title?: ReactNode;
  description?: ReactNode;
  children?: ReactNode;
  /** Right-aligned action row; Dialog.Close-wired buttons land here. */
  footer?: ReactNode;
  /** Disables escape/backdrop dismissal (approval asks). */
  blocking?: boolean;
  size?: "sm" | "md";
  className?: string;
}

export function Dialog({
  open,
  onClose,
  title,
  description,
  children,
  footer,
  blocking = false,
  size = "md",
  className,
}: DialogProps) {
  return (
    <BaseDialog.Root
      open={open}
      onOpenChange={(next) => {
        // Blocking dialogs swallow every implicit close (escape included);
        // the footer buttons call onClose() directly.
        if (!next && blocking) return;
        if (!next) onClose();
      }}
      disablePointerDismissal={blocking}
    >
      <BaseDialog.Portal>
        <BaseDialog.Backdrop className="fixed inset-0 z-50 animate-fade-in bg-canvas/70" />
        <BaseDialog.Popup
          className={cn(
            "fixed left-1/2 top-1/2 z-50 w-[calc(100vw-2rem)] -translate-x-1/2 -translate-y-1/2",
            "animate-slide-up rounded-xl bg-surface p-5 ring-1 ring-hairline shadow-overlay",
            "outline-none",
            size === "sm" ? "max-w-sm" : "max-w-md",
            className,
          )}
        >
          {title ? (
            <BaseDialog.Title className="text-base font-semibold text-ink">
              {title}
            </BaseDialog.Title>
          ) : null}
          {description ? (
            <BaseDialog.Description className="mt-1 text-sm text-dim">
              {description}
            </BaseDialog.Description>
          ) : null}
          {children ? <div className="mt-4 text-sm text-ink">{children}</div> : null}
          {footer ? <div className="mt-5 flex justify-end gap-2">{footer}</div> : null}
        </BaseDialog.Popup>
      </BaseDialog.Portal>
    </BaseDialog.Root>
  );
}

export { BaseDialog };
