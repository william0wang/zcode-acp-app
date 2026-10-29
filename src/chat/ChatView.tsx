import {
  memo,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import {
  ActionBarPrimitive,
  AuiIf,
  AssistantRuntimeProvider,
  ComposerPrimitive,
  MessagePrimitive,
  ThreadPrimitive,
  unstable_useComposerInput,
  useExternalStoreRuntime,
  useThreadViewportAutoScroll,
  useAuiState,
  groupPartByType,
  type ThreadMessageLike,
} from "@assistant-ui/react";
import ReactDiffViewer from "react-diff-viewer-continued";
import Lightbox from "yet-another-react-lightbox";
import Zoom from "yet-another-react-lightbox/plugins/zoom";
import "yet-another-react-lightbox/styles.css";
import { AUTO_EXPAND_TOP_PX, shouldAutoExpand } from "./autoExpand";
import {
  ArrowDown,
  ArrowUp,
  Bell,
  Brain,
  Check,
  ChevronDown,
  Copy,
  FilePen,
  FileText,
  Globe,
  History,
  ImagePlus,
  Layers,
  ListChecks,
  Pencil,
  Search,
  Square,
  Terminal,
  Wrench,
  X,
  Zap,
  type LucideIcon,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import "highlight.js/styles/github-dark.css";
import { useAppStore } from "../store/appStore";
import { MarkdownText } from "../components/Markdown";
import { Spinner } from "../components/Spinner";
import { MAX_IMAGES, attachmentDataUrl, prepareImage } from "../lib/image";
import { summarizeToolRun, toolNameOf, oneLine } from "./toolRun";
import type {
  AcpDiffContent,
  AttachmentDraft,
  ChatMessage,
} from "../lib/types";

// ACP chat model -> assistant-ui message model (the ACP runtime adapter).

// The runtime re-converts EVERY message on every store write (replay batches,
// streaming deltas). Without a cache each conversion mints fresh objects, so
// every message re-renders — and MarkdownText re-parses markdown + re-runs
// highlight.js for the whole list each 120ms batch, freezing taps on a mobile
// WebView for seconds. ChatMessage objects are replaced (never mutated) on
// change, so object identity is the perfect cache key.
const convertCache = new WeakMap<ChatMessage, ThreadMessageLike>();

const convertMessage = (m: ChatMessage): ThreadMessageLike => {
  const hit = convertCache.get(m);
  if (hit) return hit;
  const converted: ThreadMessageLike = {
    id: m.id,
    role: m.role,
    createdAt: new Date(m.createdAt),
    content: m.parts.map((p) => {
      if (p.type === "text") return { type: "text" as const, text: p.text };
      // Thought streams map onto the reasoning part type so they render through
      // the collapsible ThoughtCard (auto-status marks the last part of the
      // streaming message "running", which drives expand/collapse).
      if (p.type === "thought") {
        return { type: "reasoning" as const, text: p.text };
      }
      if (p.type === "image") {
        return { type: "image" as const, image: p.image };
      }
      return {
        type: "tool-call" as const,
        toolCallId: p.toolCallId,
        toolName: p.toolName,
        // args must be JSON-clean: diffs ride as a serialized array.
        args: {
          detail: p.detail,
          title: p.toolName,
          // ACP status, for the aggregated tool-run summary (the assistant-ui
          // part status says nothing about a call the app marked failed).
          status: p.status,
          ...(p.kind ? { kind: p.kind } : {}),
          ...(p.rawName ? { rawName: p.rawName } : {}),
          ...(p.foldKind ? { foldKind: p.foldKind } : {}),
          ...(p.diffs ? { diffs: JSON.stringify(p.diffs) } : {}),
        },
        result: p.status === "completed" ? p.detail : p.status,
      };
    }),
  };
  convertCache.set(m, converted);
  return converted;
};

function messageText(message: ThreadMessageLike | string): string {
  if (typeof message === "string") return message;
  if (typeof message.content === "string") return message.content;
  return message.content
    .filter((p) => p.type === "text")
    .map((p) => (p as { text: string }).text)
    .join("\n");
}

// Per-kind icon + accent colour (ADR 0004: rendering leans on libraries;
// the mapping itself is ours).
const TOOL_KIND_META: Record<string, { icon: LucideIcon; className: string }> =
  {
    execute: { icon: Terminal, className: "text-amber-400" },
    edit: { icon: FilePen, className: "text-sky-400" },
    read: { icon: FileText, className: "text-zinc-400" },
    search: { icon: Search, className: "text-violet-400" },
    fetch: { icon: Globe, className: "text-emerald-400" },
    switch_mode: { icon: ListChecks, className: "text-cyan-400" },
    other: { icon: Wrench, className: "text-faint" },
  };

// Replay harness folds (`_meta.zcode.kind`, server 0.6.0): collapsed-by-default
// plumbing cards; the summary row shows the label only, full text behind expand.
const TOOL_FOLD_META: Record<string, { icon: LucideIcon; className: string }> =
  {
    "context-handoff": { icon: Layers, className: "text-amber-400" },
    "tool-transcript": { icon: History, className: "text-zinc-400" },
  };

interface ToolCardArgs {
  detail?: string;
  kind?: string;
  rawName?: string;
  title?: string;
  status?: string;
  foldKind?: string;
  diffs?: string;
}

function parseDiffs(raw: string | undefined): AcpDiffContent[] {
  if (!raw) return [];
  try {
    const v = JSON.parse(raw) as unknown;
    return Array.isArray(v) ? (v as AcpDiffContent[]) : [];
  } catch {
    return [];
  }
}

// jsdiff (inside ReactDiffViewer) on whole-file old/new texts can freeze the
// WebView for seconds or kill it on memory — cap what we hand it and say so.
const MAX_DIFF_CHARS = 20000;

function DiffBlock({ diff }: { diff: AcpDiffContent }) {
  const { t } = useTranslation();
  const truncOld = (diff.oldText ?? "").length > MAX_DIFF_CHARS;
  const truncNew = diff.newText.length > MAX_DIFF_CHARS;
  return (
    <div className="mt-2 overflow-hidden rounded-lg ring-1 ring-hairline">
      {diff.path && (
        <div className="truncate border-b border-hairline bg-white/[0.04] px-2 py-1 font-mono text-[11px] text-dim">
          {diff.path}
        </div>
      )}
      {(truncOld || truncNew) && (
        <div className="border-b border-hairline bg-amber-950/40 px-2 py-1 text-[10px] text-amber-300">
          {t("chat.diffTruncated")}
        </div>
      )}
      <ReactDiffViewer
        oldValue={
          truncOld
            ? diff.oldText!.slice(0, MAX_DIFF_CHARS)
            : (diff.oldText ?? "")
        }
        newValue={
          truncNew ? diff.newText.slice(0, MAX_DIFF_CHARS) : diff.newText
        }
        splitView={false}
        useDarkTheme
        hideLineNumbers
        showDiffOnly={false}
        styles={{
          contentText: {
            fontSize: "11px",
            lineHeight: "1.5",
            fontFamily: "var(--font-mono, ui-monospace, monospace)",
          },
        }}
      />
    </div>
  );
}

// Copy action for an expanded ToolCard. The wire title IS the raw command
// ("Bash: npm test"), so "command" copies it whole — same text the desktop
// card's right-click "Copy command" writes.
function CardCopyButton({ label, text }: { label: string; text: string }) {
  const { t } = useTranslation();
  const notify = useAppStore((s) => s.notify);
  return (
    <button
      onClick={() => {
        void navigator.clipboard
          .writeText(text)
          .then(() => notify(t("chat.copied")))
          .catch(() => notify(t("chat.copyFailed")));
      }}
      className="rounded-full px-2 py-0.5 text-[10px] text-faint active:bg-white/[0.06]"
    >
      {label}
    </button>
  );
}

function ToolCard({ part }: { part: { toolName: string; args?: unknown } }) {
  const { t } = useTranslation();
  const a = (part.args ?? {}) as ToolCardArgs;
  const detail = a.detail ?? "";
  const diffs = useMemo(() => parseDiffs(a.diffs), [a.diffs]);
  const foldKind = typeof a.foldKind === "string" ? a.foldKind : null;
  const foldMeta = foldKind ? (TOOL_FOLD_META[foldKind] ?? null) : null;
  const meta = foldMeta ?? TOOL_KIND_META[a.kind ?? ""] ?? TOOL_KIND_META.other;
  const Icon = meta.icon;
  // `toolName` holds the wire title "Bash: npm test"; prefer the raw backend
  // name, else the segment before the first colon. Handoff folds use the
  // localized label — the wire title is English-only plumbing. The title is
  // the RAW command, so a heredoc carries newlines: flatten it to one line or
  // the summary row turns into a wall of script.
  const name =
    foldKind === "context-handoff"
      ? t("chat.contextHandoff")
      : oneLine(
          a.rawName ||
            (a.title ?? part.toolName).split(":")[0]?.trim() ||
            part.toolName,
        );
  return (
    <details className="group my-2 max-w-full overflow-hidden rounded-xl bg-white/[0.04] text-xs">
      <summary className="flex min-w-0 cursor-pointer list-none items-center gap-1.5 px-3 py-2 text-dim">
        <Icon className={`size-3.5 shrink-0 ${meta.className}`} />
        {/* shrink-0 keeps the tool name readable — an unprotected flex child
            gets squeezed to one letter by the long detail preview next to it. */}
        <span className="min-w-0 shrink-0 truncate font-medium">{name}</span>
        {/* Fold summaries stay clean: the body is harness plumbing text. */}
        {!foldKind && detail && (
          <span className="min-w-0 flex-1 truncate text-faint">
            {detail.slice(0, 120)}
          </span>
        )}
        <ChevronDown className="ml-auto size-3.5 shrink-0 text-faint transition-transform duration-200 group-open:rotate-180" />
      </summary>
      {diffs.length > 0 ? (
        <div className="border-t border-hairline px-2 py-2">
          {diffs.map((d, i) => (
            <DiffBlock key={`${d.path}-${i}`} diff={d} />
          ))}
          {detail && (
            <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap border-t border-hairline px-3 py-2 text-faint">
              {detail}
            </pre>
          )}
        </div>
      ) : (
        <pre className="max-h-64 overflow-auto whitespace-pre-wrap border-t border-hairline px-3 py-2 text-faint">
          {detail || "…"}
        </pre>
      )}
      {/* Inside <details>: rides the open state, so a collapsed card — which
          exposes nothing copyable — shows no copy affordance at all. Mobile
          has no right-click, so the desktop card menu's Copy command/output
          pair lands here as buttons. */}
      <div className="flex justify-end gap-1 border-t border-hairline px-2 py-1">
        <CardCopyButton label={t("chat.copyCommand")} text={part.toolName} />
        {detail && (
          <CardCopyButton label={t("chat.copyOutput")} text={detail} />
        )}
      </div>
    </details>
  );
}

// One run of ADJACENT tool calls folds into a single collapsed card. Without
// surrounding text each row is context-free noise (name + output head), and a
// long agent turn produces dozens — the wall the aggregated card replaces.
// Individual ToolCards stay inside, each still expandable; the caller renders
// a lone call as a plain card (no wrapper for a single row).
function ToolRunCard({
  indices,
  children,
}: {
  indices: readonly number[];
  children: ReactNode;
}) {
  const { t } = useTranslation();
  // `null` = never toggled: follow the run's own state. A live run stays open
  // so its calls are watchable as they happen; once settled it folds away. A
  // manual toggle wins permanently (the ThoughtCard rule).
  const [manual, setManual] = useState<boolean | null>(null);
  // The message's parts give the summary its names/status; the group node
  // itself carries only indices. `indices` is a fresh array per render, so
  // the memo keys on its joined identity instead of the reference.
  const parts = useAuiState((s) => s.message.parts);
  const indexKey = indices.join(",");
  const summary = useMemo(
    () =>
      summarizeToolRun(
        indexKey.split(",").flatMap((raw) => {
          const p = parts[Number(raw)];
          if (p?.type !== "tool-call") return [];
          const a = (p.args ?? {}) as ToolCardArgs;
          return [{ name: toolNameOf(a, p.toolName), status: a.status ?? "" }];
        }),
      ),
    [indexKey, parts],
  );
  const open = manual ?? summary.running;
  return (
    <div className="my-2 max-w-full overflow-hidden rounded-xl bg-white/[0.04]">
      <button
        onClick={() => setManual(!open)}
        className="flex w-full min-w-0 items-center gap-1.5 px-3 py-2 text-left"
      >
        <Wrench
          className={`size-3.5 shrink-0 ${
            summary.running
              ? "animate-pulse text-emerald-400"
              : summary.failed
                ? "text-red-400"
                : "text-faint"
          }`}
        />
        <span className="shrink-0 text-xs font-medium text-dim">
          {t("chat.toolRun", { count: summary.count })}
        </span>
        <span className="min-w-0 flex-1 truncate text-xs text-faint">
          {summary.names.join(" · ")}
        </span>
        <ChevronDown
          className={`ml-auto size-3.5 shrink-0 text-faint transition-transform duration-200 ${
            open ? "rotate-180" : ""
          }`}
        />
      </button>
      {open && (
        <div className="flex flex-col gap-1 border-t border-hairline px-2 py-1.5">
          {children}
        </div>
      )}
    </div>
  );
}

// Collapsible reasoning block. `streaming` comes from the group part's
// auto-status: it holds the block open (bottom-pinned on the newest tokens)
// while the model thinks, collapses once the answer starts, and defers to the
// first manual toggle permanently afterwards.
function ThoughtCard({
  streaming,
  children,
}: {
  streaming: boolean;
  children: ReactNode;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [touched, setTouched] = useState(false);
  const boxRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if (touched) return;
    setOpen(streaming);
  }, [streaming, touched]);

  // Bottom-pinned live preview while streaming.
  useEffect(() => {
    if (streaming && open) {
      const el = boxRef.current;
      if (el) el.scrollTop = el.scrollHeight;
    }
  });

  return (
    <div className="my-2 rounded-xl bg-white/[0.04]">
      <button
        onClick={() => {
          setTouched(true);
          setOpen((o) => !o);
        }}
        className="flex w-full items-center gap-1.5 px-3 py-2 text-left"
      >
        <Brain
          className={`size-3.5 shrink-0 ${streaming ? "animate-pulse text-dim" : "text-faint"}`}
        />
        <span className="text-xs font-medium text-dim">
          {streaming ? t("chat.thinking") : t("chat.thought")}
        </span>
        <ChevronDown
          className={`ml-auto size-3.5 shrink-0 text-faint transition-transform duration-200 ${
            open ? "" : "-rotate-90"
          }`}
        />
      </button>
      {open && (
        <div
          ref={boxRef}
          className="max-h-52 overflow-y-auto px-3 pb-2.5 text-[13px] leading-relaxed text-faint"
        >
          {children}
        </div>
      )}
    </div>
  );
}

// Harness task lifecycle notices arrive as user-role XML blocks; they are
// system plumbing, not user speech — rendered as a centered system chip.
const TASK_NOTIFICATION = /^<task-notification>[\s\S]*<\/task-notification>$/;

function parseTaskNotification(
  text: string,
): { status: string; summary: string } | null {
  const t = text.trim();
  if (!TASK_NOTIFICATION.test(t)) return null;
  const status = /<status>\s*([^<]*?)\s*<\/status>/.exec(t)?.[1] ?? "";
  const summary = /<summary>\s*([^<]*?)\s*<\/summary>/.exec(t)?.[1] ?? "";
  return { status, summary };
}

// memo: during replay every store write re-renders the list; unchanged
// messages skip re-parsing their markdown. Store-driven updates (streaming
// into the last message) still propagate via MessagePrimitive's own
// subscriptions.
const UserMessage = memo(function UserMessage({
  collapsed,
  notice,
  images,
}: {
  collapsed?: boolean;
  notice?: { status: string; summary: string };
  /** Attachment echo data URLs (resolved per store write in ChatView). */
  images?: string[];
}) {
  const { t } = useTranslation();
  const notify = useAppStore((s) => s.notify);
  const [zoom, setZoom] = useState<number | null>(null);
  // An image-only prompt has no text for the copy action to write — hide the
  // dead button (desktop parity: workbench.rs gates the same way).
  const hasText = useAuiState((s) =>
    s.message.parts.some((p) => p.type === "text" && p.text.trim() !== ""),
  );

  // Long-press copy: the bubble is plain text, so the rendered text is the
  // source (assistant-side copying uses ActionBarPrimitive on markdown source).
  const copyFrom = (e: React.MouseEvent<HTMLElement>) => {
    e.preventDefault();
    const text = e.currentTarget.innerText.trim();
    if (!text) return;
    void navigator.clipboard
      .writeText(text)
      .then(() => notify(t("chat.copied")))
      .catch(() => notify(t("chat.copyFailed")));
  };
  if (notice) {
    return (
      <MessagePrimitive.Root className="flex min-w-0 justify-center">
        <div className="flex max-w-full min-w-0 items-center gap-1.5 rounded-full border border-hairline bg-surface px-3 py-1 text-[11px] text-faint">
          <Bell className="size-3 shrink-0" />
          <span className="truncate">
            {notice.summary || t("chat.taskNotice")}
          </span>
          {notice.status && (
            <span className="shrink-0 uppercase tracking-wide">
              · {notice.status}
            </span>
          )}
        </div>
      </MessagePrimitive.Root>
    );
  }
  if (collapsed) {
    // Replay-only flag: harness-injected context handoffs render as a
    // one-line summary behind an expand control, not a wall of user text.
    return (
      <MessagePrimitive.Root className="flex min-w-0 justify-end">
        <details className="group max-w-[85%] min-w-0 rounded-[22px] rounded-br-md bg-raised px-4 py-2.5 text-[13px] text-dim">
          <summary className="flex cursor-pointer select-none list-none items-center gap-1 text-xs text-faint [&::-webkit-details-marker]:hidden">
            <ChevronDown className="size-3.5 shrink-0 transition-transform group-open:rotate-180" />
            {t("chat.contextHandoff")}
          </summary>
          <div className="mt-1.5 max-h-64 overflow-y-auto whitespace-pre-wrap leading-relaxed">
            <MessagePrimitive.Parts>
              {({ part }) => (part.type === "text" ? <>{part.text}</> : null)}
            </MessagePrimitive.Parts>
          </div>
        </details>
      </MessagePrimitive.Root>
    );
  }
  return (
    <MessagePrimitive.Root className="flex min-w-0 flex-col items-end">
      <div
        onContextMenu={copyFrom}
        className="max-w-[85%] min-w-0 break-words whitespace-pre-wrap rounded-[22px] rounded-br-md bg-raised px-4 py-2.5 text-sm text-ink select-none"
      >
        <MessagePrimitive.Parts>
          {({ part }) => (part.type === "text" ? <>{part.text}</> : null)}
        </MessagePrimitive.Parts>
        {images && images.length > 0 && (
          <div
            className={`mt-1.5 grid gap-1 ${images.length > 1 ? "grid-cols-2" : ""}`}
          >
            {images.map((src, i) => (
              <button
                key={i}
                onClick={() => setZoom(i)}
                className="overflow-hidden rounded-xl active:opacity-80"
              >
                <img
                  src={src}
                  alt=""
                  loading="lazy"
                  className={
                    images.length > 1
                      ? "h-28 w-full object-cover"
                      : "max-h-64 max-w-full object-contain"
                  }
                />
              </button>
            ))}
          </div>
        )}
      </div>
      {/* Same trailing copy action as the assistant side — the bubble is
          select-none, so the button (plus long-press) is the only copy path. */}
      {hasText && (
        <ActionBarPrimitive.Root className="mt-0.5 flex justify-end">
          <ActionBarPrimitive.Copy
            aria-label={t("chat.copy")}
            className="flex size-7 items-center justify-center rounded-full text-faint active:bg-white/[0.06] group"
          >
            <Copy className="size-3 group-data-[copied]:hidden" />
            <Check className="hidden size-3 text-emerald-400 group-data-[copied]:block" />
          </ActionBarPrimitive.Copy>
        </ActionBarPrimitive.Root>
      )}
      {zoom != null && images && (
        <Lightbox
          open
          index={zoom}
          close={() => setZoom(null)}
          slides={images.map((src) => ({ src }))}
          plugins={[Zoom]}
          zoom={{ maxZoomPixelRatio: 5 }}
        />
      )}
    </MessagePrimitive.Root>
  );
});

// Assistant replies render full-width without a bubble — markdown (code,
// lists) reads far better unconfined; only the user side gets a bubble.
// A trailing copy action (library primitive, ADR 0004) copies the message's
// markdown source.
const AssistantMessage = memo(function AssistantMessage() {
  const { t } = useTranslation();
  // The trailing copy writes the message's TEXT blocks only (thoughts and
  // tool cards stay out). A tool-calls-only message — the fresh card message
  // mid-turn — has nothing to copy, and the button would sit under collapsed
  // cards copying ""; hide it (desktop parity: workbench.rs gates the same
  // way). The cards carry their own copy actions once expanded.
  const hasText = useAuiState((s) =>
    s.message.parts.some((p) => p.type === "text" && p.text.trim() !== ""),
  );
  return (
    <MessagePrimitive.Root className="min-w-0">
      <div className="w-full text-[15px] leading-relaxed text-ink">
        <MessagePrimitive.GroupedParts
          groupBy={groupPartByType({
            reasoning: ["group-reasoning"],
            // Adjacent tool calls with no text between them read as one unit
            // of agent work: collapse the run into a single summary card.
            // Text parts are ungrouped, so they end the run naturally.
            "tool-call": ["group-tools"],
          })}
        >
          {({ part, children }) => {
            switch (part.type) {
              case "group-reasoning":
                return (
                  <ThoughtCard streaming={part.status.type === "running"}>
                    {children}
                  </ThoughtCard>
                );
              case "group-tools":
                // A single call renders bare — its own ToolCard already is
                // the summary, and a one-item wrapper would be pure chrome.
                return part.indices.length > 1 ? (
                  <ToolRunCard indices={part.indices}>{children}</ToolRunCard>
                ) : (
                  <>{children}</>
                );
              case "reasoning":
                return part.text ? (
                  <p className="whitespace-pre-wrap">{part.text}</p>
                ) : null;
              case "text":
                return part.text ? <MarkdownText text={part.text} /> : null;
              case "tool-call":
                return <ToolCard part={part} />;
              case "indicator":
                return <Spinner className="my-1 size-4" />;
              default:
                return null;
            }
          }}
        </MessagePrimitive.GroupedParts>
      </div>
      {hasText && (
        <ActionBarPrimitive.Root className="mt-0.5 flex justify-end">
          <ActionBarPrimitive.Copy
            aria-label={t("chat.copy")}
            className="flex size-7 items-center justify-center rounded-full text-faint active:bg-white/[0.06] group"
          >
            <Copy className="size-3 group-data-[copied]:hidden" />
            <Check className="hidden size-3 text-emerald-400 group-data-[copied]:block" />
          </ActionBarPrimitive.Copy>
        </ActionBarPrimitive.Root>
      )}
    </MessagePrimitive.Root>
  );
});

// Whitespace class for composer word scanning (slash-completion tokens).
const WS = /\s/;


// Touch devices have a software keyboard with no Shift key, so Enter is the
// only way to add a line: there it inserts a newline and the Send button is
// the sole submit path; pointer-fine devices keep Enter-to-send.
const COARSE_POINTER =
  typeof window !== "undefined" &&
  typeof window.matchMedia === "function" &&
  window.matchMedia("(pointer: coarse)").matches;

// Composer input row + the "/" completion menu. The command list comes from
// the bridge's available_commands_update; the query is the word under the
// caret, and every command matches at any caret position (a mid-draft
// completion inserts text — only a slash at the very start of the draft is
// executed as a command by the harness). Enter/Tab complete (caret lands
// after the inserted command), arrows navigate, Escape dismisses until the
// query changes. Keydown is intercepted in the CAPTURE phase on the form so
// it beats ComposerPrimitive.Input's Enter-send.
function Composer() {
  const { t } = useTranslation();
  const commands = useAppStore((s) => s.availableCommands);
  const isRunning = useAppStore((s) => s.isRunning);
  const sendPrompt = useAppStore((s) => s.sendPrompt);
  const forceSendPending = useAppStore((s) => s.forceSendPending);
  const notify = useAppStore((s) => s.notify);
  // Queue depth of the active session — an empty-composer Enter with a
  // non-empty queue force-sends its head instead of doing nothing.
  const pendingCount = useAppStore((s) =>
    s.activeSessionId ? (s.pendingPrompts[s.activeSessionId]?.length ?? 0) : 0,
  );
  const { value, setText } = unstable_useComposerInput();
  const [highlight, setHighlight] = useState(0);
  const [dismissedToken, setDismissedToken] = useState<string | null>(null);
  // Caret offset in the textarea; decides which word the "/" menu completes.
  const [caret, setCaret] = useState(0);
  const listRef = useRef<HTMLDivElement | null>(null);
  const inputRef = useRef<HTMLTextAreaElement | null>(null);
  // Self-tracked IME state. WebKit fires the IME-confirm key as
  // compositionend THEN keydown (isComposing=false — bugs.webkit.org
  // #165004), so neither our flag nor the native one is true when the
  // Enter keydown arrives; a short TIME window after compositionend is
  // the order-independent guard. Matching is not enough, though: the key
  // must be SWALLOWED (preventDefault + stopPropagation), because
  // ComposerPrimitive.Input's own Enter handler also believes the
  // isComposing=false lie and requestSubmit()s the message.
  const composingRef = useRef(false);
  const composeEndedAtRef = useRef(0);
  // Staged attachments (ADR 0007): compressed base64 + mime, rendered from
  // data URLs. Reset on send; ride the prompt (or the queue while running).
  const [images, setImages] = useState<AttachmentDraft[]>([]);
  const galleryRef = useRef<HTMLInputElement | null>(null);

  // The workflow "create via conversation" entry stages its prompt here
  // (server ADR-0029: prefill the draft, never auto-send). Consumed exactly
  // once — the clear must happen in the same commit, or a re-render would
  // re-apply the text over whatever the user typed in between.
  const composerPrefill = useAppStore((s) => s.composerPrefill);
  const clearComposerPrefill = useAppStore((s) => s.clearComposerPrefill);
  useEffect(() => {
    if (!composerPrefill) return;
    setText(composerPrefill.text);
    if (composerPrefill.images?.length) setImages(composerPrefill.images);
    clearComposerPrefill();
  }, [composerPrefill, setText, clearComposerPrefill]);

  // The whitespace-delimited word around the caret and its offset — the
  // completion query source. A caret resting exactly on a following word's
  // start edge counts as no word (nothing has been typed into it yet).
  let tokenStart = caret;
  while (tokenStart > 0 && !WS.test(value[tokenStart - 1]!)) tokenStart--;
  let tokenEnd = caret;
  while (tokenEnd < value.length && !WS.test(value[tokenEnd]!)) tokenEnd++;
  const token =
    tokenStart === caret && tokenEnd > caret
      ? ""
      : value.slice(tokenStart, tokenEnd);

  const matches = useMemo(() => {
    if (!token.startsWith("/")) return [];
    // Skill commands ride a "$" visual-grouping prefix (e.g. "$tdd"); the
    // wire form is "/$name", but users naturally type "/td" — strip the
    // marker on BOTH sides so either spelling matches. Insertion keeps the
    // real name (the bridge passes "/$name" through verbatim).
    const strip = (s: string) => s.replace(/^\$/, "");
    const q = strip(token.slice(1).toLowerCase());
    return commands.filter((c) => strip(c.name.toLowerCase()).startsWith(q));
  }, [token, commands]);

  // Not dismissed for this query — the caret-in-token part of the old gate is
  // inherent now that the token itself is derived from the caret position.
  const open = matches.length > 0 && dismissedToken !== token;
  const idx = Math.min(highlight, matches.length - 1);

  useEffect(() => {
    setHighlight(0);
  }, [token]);
  useEffect(() => {
    listRef.current
      ?.querySelector(`[data-idx="${idx}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }, [idx, open]);

  const select = useCallback(
    (name: string) => {
      setText(
        value.slice(0, tokenStart) +
          `/${name} ` +
          value.slice(tokenStart + token.length).trimStart(),
      );
      setDismissedToken(`/${name}`);
      // setText's value swap resets the caret to the draft's end; park it
      // right after the inserted command instead. rAF runs after React
      // commits the new value but before paint.
      const pos = tokenStart + name.length + 2;
      requestAnimationFrame(() => {
        const el = inputRef.current;
        if (el && pos <= el.value.length) {
          el.focus();
          el.setSelectionRange(pos, pos);
          setCaret(pos);
        }
      });
    },
    [setText, value, tokenStart, token],
  );

  const onKeyDownCapture = (e: React.KeyboardEvent) => {
    if (
      e.nativeEvent.isComposing ||
      composingRef.current ||
      Date.now() - composeEndedAtRef.current < 120
    ) {
      // Plain Enter is swallowed outright so the library's Enter-send never
      // sees it (see composingRef above). Shift+Enter and the composition's
      // own keys must keep flowing.
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        e.stopPropagation();
      }
      return;
    }
    if (!open) {
      if (e.key === "Enter" && !e.shiftKey) {
        if (COARSE_POINTER) {
          // Newline, not send (see COARSE_POINTER): splice "\n" in at the
          // caret, replacing any selection; the rAF caret park runs after
          // React commits the new value but before paint.
          e.preventDefault();
          e.stopPropagation();
          const el = inputRef.current;
          const start = el?.selectionStart ?? value.length;
          const end = el?.selectionEnd ?? start;
          setText(value.slice(0, start) + "\n" + value.slice(end));
          requestAnimationFrame(() => {
            const node = inputRef.current;
            if (node) {
              node.focus();
              node.setSelectionRange(start + 1, start + 1);
              setCaret(start + 1);
            }
          });
          return;
        }
        // Enter-to-send is intercepted in the capture phase (beats the
        // library's composer submit) so staged attachments ride along — the
        // library's submit path only sees text and would drop them. An
        // EMPTY composer with queued follow-ups force-sends the queue head
        // (interrupting the running turn) — the agent-client convention.
        if (!value.trim() && images.length === 0 && pendingCount > 0) {
          e.preventDefault();
          e.stopPropagation();
          forceSendPending();
        } else if (value.trim() || images.length > 0) {
          e.preventDefault();
          e.stopPropagation();
          const text = value.trim();
          setText("");
          setImages([]);
          void sendPrompt(text, images.length > 0 ? images : undefined);
        }
      }
      return;
    }
    if (e.key === "ArrowDown") {
      e.preventDefault();
      e.stopPropagation();
      setHighlight((h) => Math.min(h + 1, matches.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      e.stopPropagation();
      setHighlight((h) => Math.max(h - 1, 0));
    } else if (e.key === "Tab" || (e.key === "Enter" && !e.shiftKey)) {
      e.preventDefault();
      e.stopPropagation();
      select(matches[idx].name);
    } else if (e.key === "Escape") {
      setDismissedToken(token);
    }
  };

  const addFiles = async (files: FileList | File[]) => {
    const picked: AttachmentDraft[] = [];
    for (const f of Array.from(files)) {
      if (images.length + picked.length >= MAX_IMAGES) {
        notify(t("chat.attachLimit"));
        break;
      }
      if (!f.type.startsWith("image/")) continue;
      const res = await prepareImage(f);
      if (!res.ok) {
        notify(
          t(
            res.reason === "tooLarge"
              ? "chat.imageTooLarge"
              : "chat.attachFailed",
          ),
        );
        continue;
      }
      picked.push(res.image);
    }
    if (picked.length > 0) {
      setImages((prev) => [...prev, ...picked].slice(0, MAX_IMAGES));
    }
  };

  // Desktop-web convenience: pasted screenshots ride the same pipeline.
  const onPaste = (e: React.ClipboardEvent) => {
    const files = Array.from(e.clipboardData.files).filter((f) =>
      f.type.startsWith("image/"),
    );
    if (files.length > 0) {
      e.preventDefault();
      void addFiles(files);
    }
  };

  return (
    <ThreadPrimitive.ViewportFooter className="sticky bottom-0 bg-canvas pb-[max(var(--safe-bottom),0.5rem)] pt-2">
      {open && (
        <div
          ref={listRef}
          role="listbox"
          className="mb-2 max-h-56 overflow-y-auto rounded-2xl bg-surface py-1 shadow-2xl ring-1 ring-hairline"
        >
          {matches.map((c, i) => (
            <button
              key={c.name}
              data-idx={i}
              role="option"
              aria-selected={i === idx}
              // Keep the software keyboard open: focus stays in the textarea.
              onPointerDown={(e) => e.preventDefault()}
              onClick={() => select(c.name)}
              className={`flex w-full items-baseline gap-2 px-3 py-2 text-left ${
                i === idx ? "bg-white/[0.06]" : "active:bg-white/[0.06]"
              }`}
            >
              <span className="shrink-0 font-mono text-sm text-dim">
                /{c.name}
              </span>
              <span className="min-w-0 truncate text-xs text-faint">
                {c.description}
                {c.input?.hint ? ` — ${c.input.hint}` : ""}
              </span>
            </button>
          ))}
        </div>
      )}
      {images.length > 0 && (
        <div className="mb-2 flex gap-2 overflow-x-auto">
          {images.map((img, i) => (
            <div key={i} className="relative shrink-0">
              <img
                src={attachmentDataUrl(img)}
                alt=""
                className="size-16 rounded-xl object-cover ring-1 ring-hairline"
              />
              <button
                onClick={() =>
                  setImages((prev) => prev.filter((_, j) => j !== i))
                }
                aria-label={t("chat.remove")}
                className="absolute -right-1.5 -top-1.5 flex size-5 items-center justify-center rounded-full bg-white text-black shadow"
              >
                <X className="size-3" />
              </button>
            </div>
          ))}
        </div>
      )}
      <ComposerPrimitive.Root
        onKeyDownCapture={onKeyDownCapture}
        className="flex items-end gap-1.5 rounded-[26px] bg-raised p-1.5 pl-4 ring-1 ring-inset ring-hairline"
      >
        <input
          ref={galleryRef}
          type="file"
          accept="image/*"
          multiple
          className="hidden"
          onChange={(e) => {
            void addFiles(e.target.files ?? []);
            e.target.value = "";
          }}
        />
        {/* type="button" — inside the composer <form>, an untyped button
            defaults to submit and would fire the draft off on tap. */}
        <button
          type="button"
          onClick={() => galleryRef.current?.click()}
          aria-label={t("chat.attach")}
          className="flex size-9 shrink-0 items-center justify-center rounded-full text-dim active:bg-white/[0.06]"
        >
          <ImagePlus className="size-5" />
        </button>
        {/* leading-5 + py-2 = 36px: the single-line input matches the size-9
            buttons exactly, so items-end aligns them instead of dropping
            them below the inherited-1.5 text line. */}
        <ComposerPrimitive.Input
          ref={inputRef}
          rows={1}
          placeholder={t("chat.inputPlaceholder")}
          onPaste={onPaste}
          onCompositionStart={() => (composingRef.current = true)}
          onCompositionEnd={() => {
            composingRef.current = false;
            composeEndedAtRef.current = Date.now();
          }}
          // select covers caret moves (click, arrow keys); input covers
          // typing — Android WebView does not fire select per keystroke, and
          // a caret stuck at 0 strands the "/" menu at the draft's start
          // (the one position where 0 happens to be correct).
          onSelect={(e) => setCaret(e.currentTarget.selectionStart ?? 0)}
          onInput={(e) => setCaret(e.currentTarget.selectionStart ?? 0)}
          className="max-h-32 min-h-9 flex-1 resize-none bg-transparent py-2 text-[15px] leading-5 text-ink placeholder:text-faint focus:outline-none"
        />
        {/* While a turn runs the Send stays a Send: a draft queues as pending
            (sendPrompt routes it), never a dead button. Cancel stops the turn.
            Plain button on purpose — ComposerPrimitive.Send disables itself
            while the thread runs. */}
        {isRunning && (
          <ComposerPrimitive.Cancel
            aria-label={t("common.cancel")}
            className="flex size-9 shrink-0 items-center justify-center rounded-full bg-white text-black transition-transform active:scale-90"
          >
            <Square className="size-3 fill-current" />
          </ComposerPrimitive.Cancel>
        )}
        {(!isRunning || value.trim().length > 0 || images.length > 0) && (
          <button
            type="button"
            onClick={() => {
              const text = value.trim();
              if (!text && images.length === 0) return;
              setText("");
              setImages([]);
              void sendPrompt(text, images.length > 0 ? images : undefined);
            }}
            disabled={!value.trim() && images.length === 0}
            aria-label={t("chat.send")}
            className="flex size-9 shrink-0 items-center justify-center rounded-full bg-white text-black transition disabled:opacity-25 active:scale-90"
          >
            <ArrowUp className="size-4.5" strokeWidth={2.5} />
          </button>
        )}
      </ComposerPrimitive.Root>
    </ThreadPrimitive.ViewportFooter>
  );
}

// Queued follow-ups while a turn runs or a replay is still loading: dimmed
// user bubbles that go out by themselves when the turn settles / the session
// attaches — or right away via force-send (which interrupts the current turn).
function PendingPrompts() {
  const { t } = useTranslation();
  const activeSessionId = useAppStore((s) => s.activeSessionId);
  const pendingMap = useAppStore((s) => s.pendingPrompts);
  const forceSendPending = useAppStore((s) => s.forceSendPending);
  const discardPending = useAppStore((s) => s.discardPending);
  const editPending = useAppStore((s) => s.editPending);
  const loadingSession = useAppStore((s) => s.loadingSession);
  const connState = useAppStore((s) => s.connState);
  const lastRef = useRef<HTMLDivElement | null>(null);
  // Per-session queue (persisted across switches/restarts) — show only the
  // active session's drafts.
  const pendingPrompts =
    (activeSessionId != null ? pendingMap[activeSessionId] : undefined) ?? [];

  useEffect(() => {
    lastRef.current?.scrollIntoView({ block: "end" });
  }, [pendingPrompts.length]);

  if (pendingPrompts.length === 0) return null;
  return (
    <>
      {pendingPrompts.map((draft, i) => (
        <div
          key={i}
          ref={i === pendingPrompts.length - 1 ? lastRef : undefined}
          className="flex min-w-0 justify-end opacity-60"
        >
          <div className="flex max-w-[85%] min-w-0 flex-col break-words whitespace-pre-wrap rounded-[22px] rounded-br-md bg-raised px-4 py-2.5 text-sm text-ink">
            {draft.text && <span>{draft.text}</span>}
            {draft.images.length > 0 && (
              <div className="mt-1 flex gap-1.5">
                {draft.images.map((img, j) => (
                  <img
                    key={j}
                    src={attachmentDataUrl(img)}
                    alt=""
                    className="size-12 rounded-lg object-cover ring-1 ring-hairline"
                  />
                ))}
              </div>
            )}
            <span className="mt-1 flex items-center justify-end gap-3 text-[11px]">
              <span className="text-faint">{t("chat.pending")}</span>
              {/* No force-send while replaying (no turn to interrupt — the
                  queue fires on its own once the attach lands) or while
                  disconnected (the post-reconnect flush owns the queue). */}
              {!loadingSession && connState === "open" && (
                <button
                  onClick={() => forceSendPending(i)}
                  className="flex items-center gap-1 font-medium text-dim active:text-ink"
                >
                  <Zap className="size-3" />
                  {t("chat.sendNow")}
                </button>
              )}
              <button
                onClick={() => editPending(i)}
                aria-label={t("chat.editPending")}
                className="flex size-4 items-center justify-center text-faint"
              >
                <Pencil className="size-3" />
              </button>
              <button
                onClick={() => discardPending(i)}
                aria-label={t("chat.discard")}
                className="flex size-4 items-center justify-center text-faint"
              >
                <X className="size-3" />
              </button>
            </span>
          </div>
        </div>
      ))}
    </>
  );
}

// Distance from the scroll bottom within which the viewport counts as
// "at the bottom" for the jump button and unread reset.
const NEAR_BOTTOM_PX = 64;

function Thread({
  viewportRef,
  onScroll,
  hasMore,
  remaining,
  loadingEarlier,
  onExpand,
  collapsedIds,
  taskNotices,
  userImages,
}: {
  viewportRef: React.RefObject<HTMLDivElement | null>;
  onScroll: () => void;
  hasMore: boolean;
  remaining: number | null;
  loadingEarlier: boolean;
  onExpand: () => void;
  collapsedIds: ReadonlySet<string>;
  taskNotices: ReadonlyMap<string, { status: string; summary: string }>;
  userImages: ReadonlyMap<string, string[]>;
}) {
  const { t } = useTranslation();
  const activeSessionId = useAppStore((s) => s.activeSessionId);
  const loadingSession = useAppStore((s) => s.loadingSession);
  const messages = useAppStore((s) => s.messages);

  // Stick-to-bottom comes from the library hook (ADR 0004); only the
  // load-earlier anchoring stays hand-rolled below. The library's
  // isAtBottom signal only flips within 1px of the exact bottom — mobile
  // flings routinely rest a few px short, which would leave the jump
  // button stuck over the last message. Measure locally with a tolerant
  // threshold instead.
  const autoScrollRef = useThreadViewportAutoScroll({ autoScroll: true });
  const [atBottom, setAtBottom] = useState(true);
  const measure = useCallback(() => {
    const el = viewportRef.current;
    if (!el) return;
    const distance = el.scrollHeight - el.scrollTop - el.clientHeight;
    setAtBottom(
      distance <= NEAR_BOTTOM_PX || el.scrollHeight <= el.clientHeight,
    );
  }, [viewportRef]);
  useEffect(() => {
    const el = viewportRef.current;
    if (!el) return;
    el.addEventListener("scroll", measure, { passive: true });
    return () => el.removeEventListener("scroll", measure);
  }, [viewportRef, measure]);
  // Session switches swap content without firing a scroll event —
  // re-measure a frame after the write so the library's own
  // bottom-restore (ResizeObserver, same frame) has landed first.
  useEffect(() => {
    const id = requestAnimationFrame(measure);
    return () => cancelAnimationFrame(id);
  }, [messages, measure]);

  // Unread count: messages appended while the viewport is away from the
  // bottom. Prepended history pages are not unread content.
  const [unread, setUnread] = useState(0);
  const lastLenRef = useRef(messages.length);
  useEffect(() => {
    const grew = messages.length - lastLenRef.current;
    lastLenRef.current = messages.length;
    if (atBottom) {
      setUnread(0);
      return;
    }
    if (grew > 0 && !loadingEarlier) setUnread((u) => u + grew);
  }, [messages, atBottom, loadingEarlier]);

  const composeRef = useCallback(
    (el: HTMLDivElement | null) => {
      viewportRef.current = el;
      autoScrollRef(el);
    },
    [viewportRef, autoScrollRef],
  );

  return (
    <ThreadPrimitive.Root className="relative flex h-full flex-col">
      <ThreadPrimitive.Viewport
        ref={composeRef}
        onScroll={onScroll}
        className="flex flex-1 flex-col gap-4 overflow-y-auto overflow-x-hidden px-3.5 py-4"
      >
        <AuiIf condition={(s) => s.thread.isEmpty}>
          <div className="mt-[22vh] flex flex-col items-center gap-3 text-center">
            <h1 className="text-2xl font-semibold tracking-tight text-ink">
              ZCode
            </h1>
            <p className="max-w-64 text-sm text-faint">
              {loadingSession
                ? t("chat.loadingSession")
                : activeSessionId
                  ? t("chat.empty")
                  : t("chat.noSession")}
            </p>
            {loadingSession && <Spinner className="size-5" />}
          </div>
        </AuiIf>
        {hasMore && (
          <button
            onClick={onExpand}
            disabled={loadingEarlier}
            className="mx-auto shrink-0 rounded-full bg-surface px-3 py-1 text-xs text-dim ring-1 ring-hairline active:bg-white/[0.06] disabled:opacity-50"
          >
            {loadingEarlier
              ? t("chat.loadingEarlier")
              : remaining != null
                ? t("chat.loadEarlier", { n: remaining })
                : t("chat.loadEarlierGeneric")}
          </button>
        )}
        <ThreadPrimitive.Messages>
          {({ message }) =>
            message.role === "user" ? (
              <UserMessage
                key={message.id}
                collapsed={collapsedIds.has(message.id)}
                notice={taskNotices.get(message.id)}
                images={userImages.get(message.id)}
              />
            ) : (
              <AssistantMessage key={message.id} />
            )
          }
        </ThreadPrimitive.Messages>
        <PendingPrompts />
        <Composer />
      </ThreadPrimitive.Viewport>
      {!atBottom && messages.length > 0 && (
        <div className="pointer-events-none absolute inset-x-0 bottom-24 z-10 flex justify-end px-4">
          <div className="pointer-events-auto relative">
            <ThreadPrimitive.ScrollToBottom
              aria-label={t("chat.jumpToLatest")}
              className="flex size-9 items-center justify-center rounded-full bg-raised text-dim shadow-lg ring-1 ring-hairline transition-transform active:scale-90"
            >
              <ArrowDown className="size-4" />
            </ThreadPrimitive.ScrollToBottom>
            {unread > 0 && (
              <span className="absolute -right-1 -top-1 flex size-4 min-w-4 items-center justify-center rounded-full bg-blue-500 px-1 text-[10px] font-semibold text-white">
                {unread > 99 ? "99+" : unread}
              </span>
            )}
          </div>
        </div>
      )}
    </ThreadPrimitive.Root>
  );
}

export function ChatView() {
  const messages = useAppStore((s) => s.messages);
  const isRunning = useAppStore((s) => s.isRunning);
  const sendPrompt = useAppStore((s) => s.sendPrompt);
  const cancelTurn = useAppStore((s) => s.cancelTurn);
  const hasMore = useAppStore((s) => s.hasMore);
  const totalMessages = useAppStore((s) => s.totalMessages);
  const loadingEarlier = useAppStore((s) => s.loadingEarlier);
  const loadEarlier = useAppStore((s) => s.loadEarlier);
  const activeSessionId = useAppStore((s) => s.activeSessionId);

  const remaining =
    totalMessages != null ? Math.max(0, totalMessages - messages.length) : null;
  const viewportRef = useRef<HTMLDivElement | null>(null);
  const expandingRef = useRef(false);

  // Auto load-earlier must key off GENUINE user input only: programmatic
  // scrolls (the prepend anchor-restore, the entry content swap, the
  // stick-to-bottom hook) also fire scroll events, and level-triggering on
  // "scrollTop < threshold" re-armed on those — cascading 2+ page fetches
  // per touch and on every session re-entry (shouldAutoExpand, edge-fired).
  const lastInputRef = useRef(0);
  const wasAtTopRef = useRef(false);
  useEffect(() => {
    const el = viewportRef.current;
    if (!el) return;
    const mark = () => {
      lastInputRef.current = Date.now();
    };
    el.addEventListener("pointerdown", mark, { passive: true });
    el.addEventListener("wheel", mark, { passive: true });
    el.addEventListener("touchstart", mark, { passive: true });
    el.addEventListener("keydown", mark);
    return () => {
      el.removeEventListener("pointerdown", mark);
      el.removeEventListener("wheel", mark);
      el.removeEventListener("touchstart", mark);
      el.removeEventListener("keydown", mark);
    };
  }, []);
  // A new session repaints the viewport: forget the old session's input
  // stamps and top-zone state so the entry swap cannot arm the trigger.
  useEffect(() => {
    lastInputRef.current = 0;
    wasAtTopRef.current = false;
  }, [activeSessionId]);

  // Older history arrives as PREPENDED pages (session/load_earlier). Anchor
  // the viewport: sample until the content height actually grew (network +
  // assistant-ui's async mount both take frames), then restore the exact
  // visual position. (Live-stream stick-to-bottom is the library hook.)
  const expand = useCallback(() => {
    if (expandingRef.current) return;
    expandingRef.current = true;
    const el = viewportRef.current;
    const prevHeight = el ? el.scrollHeight : 0;
    const prevScrollTop = el ? el.scrollTop : 0;
    void loadEarlier().then((applied) => {
      expandingRef.current = false;
      if (!applied || !el) return;
      const deadline = performance.now() + 10000;
      const restore = () => {
        if (el.scrollHeight > prevHeight) {
          el.scrollTop = prevScrollTop + (el.scrollHeight - prevHeight);
          return;
        }
        if (performance.now() < deadline) requestAnimationFrame(restore);
      };
      requestAnimationFrame(restore);
    });
  }, [loadEarlier]);

  const onScroll = useCallback(() => {
    const el = viewportRef.current;
    if (!el) return;
    const atTop = el.scrollTop < AUTO_EXPAND_TOP_PX && el.scrollHeight > el.clientHeight;
    const fire = shouldAutoExpand({
      atTop,
      wasAtTop: wasAtTopRef.current,
      sinceInputMs: Date.now() - lastInputRef.current,
      expanding: expandingRef.current,
    });
    wasAtTopRef.current = atTop;
    if (fire) expand();
  }, [expand]);

  const onNew = useCallback(
    async (message: ThreadMessageLike | string) => {
      await sendPrompt(messageText(message));
    },
    [sendPrompt],
  );
  const onCancel = useCallback(async () => {
    cancelTurn();
  }, [cancelTurn]);

  // Collapsed (context-handoff) message ids, resolved once per store write so
  // the message components keep stable boolean props and stay memoized.
  const collapsedIds = useMemo(
    () => new Set(messages.filter((m) => m.collapsed).map((m) => m.id)),
    [messages],
  );

  // Task-notification ids parsed the same way — one pass per store write,
  // stable object identity keeps UserMessage memoization intact.
  const taskNotices = useMemo(() => {
    const map = new Map<string, { status: string; summary: string }>();
    for (const m of messages) {
      if (m.role !== "user") continue;
      const text = m.parts
        .map((p) => (p.type === "text" ? p.text : ""))
        .join("");
      const notice = parseTaskNotification(text);
      if (notice) map.set(m.id, notice);
    }
    return map;
  }, [messages]);

  // Attachment echo data URLs per user message — same stable-identity pattern.
  const userImages = useMemo(() => {
    const map = new Map<string, string[]>();
    for (const m of messages) {
      if (m.role !== "user") continue;
      const urls = m.parts
        .filter((p) => p.type === "image")
        .map((p) => (p as { image: string }).image);
      if (urls.length > 0) map.set(m.id, urls);
    }
    return map;
  }, [messages]);

  const runtime = useExternalStoreRuntime({
    messages,
    convertMessage,
    onNew,
    onCancel,
    isRunning,
  });

  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <Thread
        viewportRef={viewportRef}
        onScroll={onScroll}
        hasMore={hasMore}
        remaining={remaining}
        loadingEarlier={loadingEarlier}
        onExpand={expand}
        collapsedIds={collapsedIds}
        taskNotices={taskNotices}
        userImages={userImages}
      />
    </AssistantRuntimeProvider>
  );
}
