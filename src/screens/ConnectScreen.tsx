import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, ChevronLeft, Pencil, Plus } from "lucide-react";
import { HubClient } from "../lib/hub";
import { useBackHandler } from "../lib/backNav";
import { useAppStore } from "../store/appStore";
import type { SavedServer } from "../lib/types";

const inputClass =
  "mt-1 w-full rounded-xl bg-raised px-4 py-3 text-sm text-ink ring-1 ring-inset ring-hairline placeholder:text-faint focus:ring-2 focus:ring-ink/40 focus:outline-none";

// Same-origin mode (the hub serves the web build itself): an empty URL means
// "this page's origin". Only a real browser over http(s) qualifies — the
// Tauri shell is excluded (its Windows/Android origin IS http(s), but it is
// tauri.localhost, not a hub).
function sameOriginHubUrl(): string | null {
  if (typeof window === "undefined") return null;
  if ("__TAURI_INTERNALS__" in window) return null;
  const { protocol, hostname } = window.location;
  if (protocol !== "http:" && protocol !== "https:") return null;
  if (hostname === "tauri.localhost") return null;
  return window.location.origin;
}

// Add/edit form. ADDING keeps the original gate — health + discovery must
// answer before a new server is saved. EDITING saves in place (a down hub is
// fine; the offline banner + polling handle it) and only the ACTIVE server
// reconnects immediately, from the store side.
function ServerForm({
  server,
  onCancel,
}: {
  server: SavedServer | null;
  // Null on a fresh install (nothing to go back to): the cancel button hides.
  onCancel: (() => void) | null;
}) {
  const { t } = useTranslation();
  const connectToHub = useAppStore((s) => s.connectToHub);
  const saveServer = useAppStore((s) => s.saveServer);
  const deleteServer = useAppStore((s) => s.deleteServer);
  const [name, setName] = useState(server?.name ?? "");
  const [hubUrl, setHubUrl] = useState(server?.hubUrl ?? "");
  const [token, setToken] = useState(server?.token ?? "");
  const [error, setError] = useState<string | null>(null);
  const [testing, setTesting] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const sameOrigin = sameOriginHubUrl();

  // The confirm state is fleeting — a stray first tap must not linger armed.
  useEffect(() => {
    if (!confirmDelete) return;
    const id = setTimeout(() => setConfirmDelete(false), 3000);
    return () => clearTimeout(id);
  }, [confirmDelete]);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setError(null);
    const url = hubUrl.trim()
      ? hubUrl.trim().replace(/\/+$/, "")
      : sameOrigin;
    const secret = token.trim();
    if (!url) {
      setError(t("connect.invalidUrl"));
      return;
    }
    try {
      const parsed = new URL(url);
      if (parsed.protocol !== "http:" && parsed.protocol !== "https:")
        throw new Error();
    } catch {
      setError(t("connect.invalidUrl"));
      return;
    }
    if (server) {
      saveServer({ ...server, name: name.trim(), hubUrl: url, token: secret });
      onCancel?.();
      return;
    }
    setTesting(true);
    try {
      const client = new HubClient(url, secret);
      await client.health();
      await client.instances();
      connectToHub({ hubUrl: url, token: secret, name: name.trim() });
      onCancel?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setTesting(false);
    }
  }

  return (
    <form onSubmit={submit} className="mt-8">
      <h2 className="text-base font-semibold">
        {server ? t("connect.editServer") : t("connect.addServer")}
      </h2>

      <label className="mt-4 block text-xs font-medium text-dim">
        {t("connect.serverName")}
      </label>
      <input
        value={name}
        onChange={(e) => setName(e.target.value)}
        placeholder={t("connect.namePlaceholder")}
        autoCapitalize="none"
        autoCorrect="off"
        className={inputClass}
      />

      <label className="mt-4 block text-xs font-medium text-dim">
        {t("connect.hubUrl")}
      </label>
      <input
        value={hubUrl}
        onChange={(e) => setHubUrl(e.target.value)}
        placeholder={
          sameOrigin ? t("connect.urlPlaceholderSameOrigin") : "https://hub.example.com"
        }
        autoCapitalize="none"
        autoCorrect="off"
        inputMode="url"
        className={inputClass}
      />

      <label className="mt-4 block text-xs font-medium text-dim">
        {t("connect.token")}
      </label>
      <input
        value={token}
        onChange={(e) => setToken(e.target.value)}
        type="password"
        autoCapitalize="none"
        autoCorrect="off"
        className={inputClass}
      />

      {error && (
        <p className="mt-4 rounded-lg border border-danger/30 bg-danger-wash px-3 py-2 text-xs text-danger">
          {error}
        </p>
      )}

      <button
        type="submit"
        disabled={testing || (!hubUrl.trim() && !sameOrigin) || !token.trim()}
        className="mt-6 w-full rounded-xl bg-white py-3 text-sm font-semibold text-black transition active:scale-[0.99] disabled:opacity-40"
      >
        {testing
          ? t("connect.testing")
          : server
            ? t("connect.saveServer")
            : t("connect.connect")}
      </button>

      {server && (
        <button
          type="button"
          onClick={() => {
            if (confirmDelete) {
              deleteServer(server.id);
              onCancel?.();
            } else setConfirmDelete(true);
          }}
          className={`mt-2 w-full rounded-xl px-3 py-2 text-xs font-medium transition ${
            confirmDelete
              ? "bg-danger-strong text-white active:brightness-90"
              : "bg-danger-wash text-danger active:brightness-125"
          }`}
        >
          {confirmDelete
            ? t("connect.deleteConfirm")
            : t("connect.deleteServer")}
        </button>
      )}

      {onCancel && (
        <button
          type="button"
          onClick={onCancel}
          className="mt-2 w-full rounded-xl px-3 py-2 text-xs font-medium text-dim hover:bg-hover active:bg-active"
        >
          {t("common.cancel")}
        </button>
      )}
    </form>
  );
}

// Server manager: lists every saved server (tap a row to switch, pencil to
// edit/delete) plus the add form. A fresh install lands directly on the add
// form — which is the original single-server connect screen. With onClose
// (settings' "manage servers" entry) it renders over a LIVE connection and
// the back button returns to the app without disconnecting.
export function ConnectScreen({ onClose }: { onClose?: () => void }) {
  const { t } = useTranslation();
  const savedServers = useAppStore((s) => s.savedServers);
  const activeServerId = useAppStore((s) => s.activeServerId);
  const switchServer = useAppStore((s) => s.switchServer);
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState<SavedServer | null>(null);

  const closeForm = () => {
    setAdding(false);
    setEditing(null);
  };
  const freshInstall = savedServers.length === 0;

  // Back mirrors the on-screen controls: out of an open add/edit form first,
  // then out of the manager overlay. The root connect screen falls through to
  // the app-level double-back-to-exit.
  useBackHandler(() => {
    if (adding || editing) {
      closeForm();
      return true;
    }
    if (onClose) {
      onClose();
      return true;
    }
    return false;
  });

  return (
    <div className="h-full overflow-y-auto bg-canvas px-6 text-ink">
      <div className="relative mx-auto w-full max-w-sm py-10">
        {onClose && (
          <button
            onClick={onClose}
            aria-label={t("common.back")}
            className="absolute left-0 top-10 flex size-9 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active"
          >
            <ChevronLeft className="size-5" />
          </button>
        )}
        <h1 className="text-center text-2xl font-semibold tracking-tight">
          ZCode ACP
        </h1>
        <p className="mt-2 text-center text-sm text-faint">
          {t("connect.subtitle")}
        </p>
        <p className="mt-1 text-center text-2xs text-faint">
          v{__APP_VERSION__}
        </p>

        {adding || editing ? (
          <ServerForm
            key={editing?.id ?? "new"}
            server={editing}
            onCancel={freshInstall ? null : closeForm}
          />
        ) : freshInstall ? (
          <ServerForm server={null} onCancel={null} />
        ) : (
          <>
            <h3 className="mt-8 text-xs font-medium text-dim">
              {t("connect.savedServers")}
            </h3>
            <div className="mt-2 space-y-2">
              {savedServers.map((s) => (
                <div
                  key={s.id}
                  className="flex items-center rounded-xl bg-raised ring-1 ring-inset ring-hairline"
                >
                  <button
                    onClick={() => switchServer(s.id)}
                    className="flex min-w-0 flex-1 items-center gap-2 rounded-xl px-4 py-3 text-left hover:bg-hover active:bg-active"
                  >
                    <span className="min-w-0 flex-1">
                      <span className="block truncate text-sm font-medium">
                        {s.name}
                      </span>
                      <span className="block truncate font-mono text-xs text-faint">
                        {s.hubUrl}
                      </span>
                    </span>
                    {s.id === activeServerId && (
                      <Check className="size-4 shrink-0 text-dim" />
                    )}
                  </button>
                  <button
                    onClick={() => setEditing(s)}
                    aria-label={t("connect.editServer")}
                    className="mr-1 flex size-9 shrink-0 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active"
                  >
                    <Pencil className="size-4" />
                  </button>
                </div>
              ))}
            </div>
            <button
              onClick={() => setAdding(true)}
              className="mt-4 flex w-full items-center justify-center gap-2 rounded-xl border border-dashed border-hairline px-3 py-2.5 text-xs font-medium text-dim hover:bg-hover active:bg-active"
            >
              <Plus className="size-4" />
              {t("connect.addServer")}
            </button>
          </>
        )}
      </div>
    </div>
  );
}
