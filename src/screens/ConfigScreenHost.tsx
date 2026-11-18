import { lazy, Suspense } from "react";
import { Skeleton } from "../ui";
import { useAppStore } from "../store/appStore";

// The configuration screens (ADR-0009) are ~1.7k lines across ten files and
// are opened rarely — from the settings panel or the instance picker. Loading
// them with the first paint would put them in the bundle every user pays for
// on every launch, so they are split the same way ChatScreen is: the chunk
// arrives when the screen is first opened.
const ZCodeConfigScreen = lazy(() =>
  import("./ZCodeConfigScreen").then((m) => ({ default: m.ZCodeConfigScreen })),
);

/**
 * Mounts the configuration screen when the store says it is open.
 *
 * Both entry points (the settings panel inside a session, and the instance
 * picker header) render this and nothing else, so the open state lives in one
 * place and the two cannot disagree about who owns the screen. The null guard
 * makes it safe to mount unconditionally too, and covers the moment between
 * `closeConfig` and the lazy chunk unmounting.
 */
export function ConfigScreenHost() {
  const configOpen = useAppStore((s) => s.configOpen);
  if (!configOpen) return null;
  return (
    <Suspense
      fallback={
        // Chunk fallback: settings-page-shaped blocks (header row + list
        // lines), not a spinner.
        <div className="flex h-full flex-col gap-4 bg-canvas px-4 pt-[max(var(--safe-top),1rem)]">
          <Skeleton className="h-6 w-1/3" />
          {[0.85, 0.6, 0.75, 0.5, 0.7].map((w, i) => (
            <div key={i} className="flex flex-col gap-2 rounded-xl bg-raised p-3">
              <Skeleton className="h-3.5" style={{ width: `${w * 100}%` }} />
              <Skeleton className="h-3" style={{ width: `${w * 55}%` }} />
            </div>
          ))}
        </div>
      }
    >
      <ZCodeConfigScreen />
    </Suspense>
  );
}
