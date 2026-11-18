import { memo, useRef, useState, type ReactNode } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import { useTranslation } from "react-i18next";
import { Check, Copy } from "lucide-react";
import { canOpenLocally, fileUrlToPath, openLocalPath } from "../lib/localOpen";

// file:// links never navigate the webview (it cannot render local files
// anyway). When the hub is loopback — the desktop app on the same machine —
// the target is a real local file: hand it to the OS instead.
function LocalAwareAnchor({ href, children }: { href?: string; children?: ReactNode }) {
  if (href?.startsWith("file://")) {
    return (
      <a
        href={href}
        onClick={(e) => {
          e.preventDefault();
          const p = fileUrlToPath(href);
          if (p && canOpenLocally()) {
            void openLocalPath(p, false).catch(() => undefined);
          }
        }}
      >
        {children}
      </a>
    );
  }
  return <a href={href}>{children}</a>;
}

// Code block copy (requirements 0.2). The raw text is read from the DOM at
// click time — rehype-highlight nests spans per token, so reconstructing
// from React children would re-walk the same tree by hand.
function CodeBlock({ children }: { children?: ReactNode }) {
  const { t } = useTranslation();
  const preRef = useRef<HTMLPreElement | null>(null);
  const [copied, setCopied] = useState(false);
  const onCopy = () => {
    const el = preRef.current;
    if (!el) return;
    // Strip the single trailing newline the browser adds to <pre> layout.
    const text = el.innerText.replace(/\n$/, "");
    if (!text) return;
    void navigator.clipboard
      .writeText(text)
      .then(() => {
        setCopied(true);
        setTimeout(() => setCopied(false), 1500);
      })
      .catch(() => undefined);
  };
  return (
    <div className="group relative">
      <button
        onClick={onCopy}
        aria-label={t("chat.copyCode")}
        // Always visible on touch (no hover to reveal it); on hover-capable
        // pointers it stays out of the way until the block is hovered.
        className="absolute right-2 top-2 z-10 flex size-7 items-center justify-center rounded-lg bg-surface/80 text-faint ring-1 ring-hairline backdrop-blur transition-opacity hover:bg-hover active:bg-active [@media(hover:hover)]:opacity-0 [@media(hover:hover)]:group-hover:opacity-100 [@media(hover:hover)]:focus-visible:opacity-100"
      >
        {copied ? (
          <Check className="size-3.5 text-success" />
        ) : (
          <Copy className="size-3.5" />
        )}
      </button>
      <pre ref={preRef}>{children}</pre>
    </div>
  );
}

// Shared by chat messages and the file viewer's markdown preview.
// memo: markdown + highlight parsing is the most expensive render in the app;
// identical text must never re-parse (belt-and-suspenders on top of the
// convertMessage identity cache in ChatView).
export const MarkdownText = memo(function MarkdownText({
  text,
}: {
  text: string;
}) {
  return (
    <div className="prose prose-sm prose-invert max-w-none prose-pre:rounded-xl prose-pre:bg-canvas prose-pre:ring-1 prose-pre:ring-hairline">
      <Markdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[rehypeHighlight]}
        components={{ a: LocalAwareAnchor, pre: CodeBlock }}
      >
        {text}
      </Markdown>
    </div>
  );
});
