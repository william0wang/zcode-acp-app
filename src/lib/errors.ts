import i18n from "../i18n";
import { HubApiError } from "./hub";

/**
 * The typed-error mapping the desktop client's HubApiError spells (the
 * settings error doctrine): a failed fetch is an expected offline state, 401
 * is check-token, and anything else shows the error's own message VERBATIM —
 * a refused write's body is the message, and dropping it leaves "HTTP 409"
 * with no way forward.
 *
 * Reads the i18n instance directly: the stores call this outside React.
 */
export function describeHubError(e: unknown): string {
  if (e instanceof HubApiError) {
    if (e.network) return i18n.t("notice.hubOffline");
    if (e.status === 401) return i18n.t("notice.hubUnauthorized");
  }
  return e instanceof Error ? e.message : String(e);
}

/** The write wrappers' no-client verdict: nothing was sent (the connection
 * dropped mid-flow), which is not an error object — it is its own line. */
export function notConnectedMessage(): string {
  return i18n.t("notice.notConnected");
}
