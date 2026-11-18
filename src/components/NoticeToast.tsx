import { useAppStore } from "../store/appStore";

// Ephemeral feedback layer rendering ABOVE every z-50 full-screen overlay
// (FileBrowser, FileViewer, dialogs): toasts raised inside those screens —
// download started/done/failed, copy and share results — were previously
// parked on the notice banner, which those overlays hide entirely.
export function NoticeToast() {
  const toast = useAppStore((s) => s.toast);
  const dismissToast = useAppStore((s) => s.dismissToast);
  if (!toast) return null;
  return (
    <div className="pointer-events-none fixed inset-x-0 bottom-0 z-[100] flex justify-center p-4 pb-[max(var(--safe-bottom),1rem)]">
      <button
        onClick={dismissToast}
        className="pointer-events-auto max-w-full truncate rounded-full bg-surface px-4 py-2 text-xs text-ink shadow-lg ring-1 ring-hairline hover:bg-hover active:bg-active"
      >
        {toast.text}
      </button>
    </div>
  );
}
