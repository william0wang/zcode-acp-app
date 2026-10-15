import { memo, type ReactNode } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
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
        components={{ a: LocalAwareAnchor }}
      >
        {text}
      </Markdown>
    </div>
  );
});
