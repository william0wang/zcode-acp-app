import { lazy, Suspense, useEffect } from "react";
import type { ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { onBackButtonPress } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { dispatchBack } from "./lib/backNav";
import { useAppStore } from "./store/appStore";
import { ConnectScreen } from "./screens/ConnectScreen";
import { InstancePicker } from "./screens/InstancePicker";
import { NoticeToast } from "./components/NoticeToast";
import { Spinner } from "./components/Spinner";
import { TerminalScreen } from "./components/TerminalScreen";

// ChatScreen carries the chat engine + diff/lightbox/highlight stack (~63%
// of the bundle); the connect/picker screens only pay for it once a session
// actually opens. In the APK the chunk loads from local disk, on the web
// deployment this cuts first paint from ~406 KB to ~150 KB gzipped.
const ChatScreen = lazy(() =>
  import("./screens/ChatScreen").then((m) => ({ default: m.ChatScreen })),
);

export default function App() {
  const { t } = useTranslation();
  const init = useAppStore((s) => s.init);
  const profile = useAppStore((s) => s.profile);
  const manageOpen = useAppStore((s) => s.manageOpen);
  const closeServerManager = useAppStore((s) => s.closeServerManager);
  const instanceId = useAppStore((s) => s.instanceId);
  const activeSessionId = useAppStore((s) => s.activeSessionId);
  const terminalTarget = useAppStore((s) => s.terminalTarget);

  useEffect(() => {
    init();
  }, [init]);

  // Android back gesture: the overlay/screen handler stack (backNav) owns it
  // while anything can navigate back internally. At the root screen the press
  // arms a 2s window instead — a second press inside it exits (the standard
  // double-back-to-exit contract; a single stray gesture never kills the
  // app). Registered only inside the Tauri shell; the web build keeps the
  // browser's own back behavior. Registering the listener at all is what
  // suppresses Tauri's default (webview-history back, then exit).
  useEffect(() => {
    if (typeof window === "undefined" || !("__TAURI_INTERNALS__" in window))
      return;
    let lastPress = 0;
    let unlisten: (() => void) | null = null;
    let alive = true;
    onBackButtonPress(() => {
      if (dispatchBack()) return;
      const now = Date.now();
      if (now - lastPress < 2000) {
        lastPress = 0;
        void invoke("exit_app");
      } else {
        lastPress = now;
        useAppStore.getState().notify(t("common.exitHint"));
      }
    }).then(
      (l) => {
        if (alive) unlisten = () => void l.unregister();
        else void l.unregister();
      },
      // Desktop shell: no android back events exist there.
      () => {},
    );
    return () => {
      alive = false;
      unlisten?.();
    };
  }, [t]);

  let body: ReactNode;
  if (!profile) body = <ConnectScreen />;
  // Server manager overlay: browses/edits saved servers WITHOUT dropping the
  // live connection; switching or adding a server clears manageOpen itself.
  else if (manageOpen) body = <ConnectScreen onClose={closeServerManager} />;
  // The instance connection outlives the open session (closeSession keeps
  // it so the list keeps receiving broadcast activity) — the chat screen
  // needs BOTH an instance and an attached session.
  else if (!instanceId || !activeSessionId) body = <InstancePicker />;
  else
    body = (
      <Suspense
        fallback={
          <div className="flex h-full items-center justify-center bg-canvas">
            <Spinner className="size-6" />
          </div>
        }
      >
        <ChatScreen />
      </Suspense>
    );
  // Toasts live above every screen (including the z-50 file overlays); the
  // Remote Terminal overlay sits above the screens but below the dialogs.
  return (
    <>
      {body}
      {terminalTarget && <TerminalScreen />}
      <NoticeToast />
    </>
  );
}
