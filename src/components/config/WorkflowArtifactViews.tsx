import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { WorkflowArtifact } from "../../lib/types";

/**
 * Structured renderers for the preset artifact kinds (chart / table / metrics /
 * board) plus the shared fallbacks. The bridge passes the declaration specs
 * through untouched (the protocol does not restate their shapes — the four
 * renderers interpret them, upstream workflow-artifacts.ts), so every spec
 * read here is defensive: a malformed or absent spec degrades to a key-value
 * card list, and the raw JSON stays available behind an explicit toggle
 * instead of being the default view.
 */

/** Pretty JSON for the viewers, capped so a huge payload cannot wedge the DOM. */
const JSON_CAP = 20_000;
export function prettyJson(value: unknown): string {
  const text = (() => {
    try {
      return JSON.stringify(value, null, 2) ?? "null";
    } catch {
      return String(value);
    }
  })();
  return text.length > JSON_CAP ? text.slice(0, JSON_CAP) + "\n…" : text;
}

/** Short cell text: scalars verbatim, objects as a one-line compact JSON. */
export function cellText(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "object") {
    try {
      const s = JSON.stringify(value);
      return s === undefined ? String(value) : s.length > 80 ? `${s.slice(0, 79)}…` : s;
    } catch {
      return String(value);
    }
  }
  return String(value);
}

interface FieldSpec {
  field: string;
  label?: string;
  unit?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function parseField(value: unknown): FieldSpec | null {
  if (!isRecord(value) || typeof value.field !== "string" || !value.field) return null;
  return {
    field: value.field,
    ...(typeof value.label === "string" ? { label: value.label } : {}),
    ...(typeof value.unit === "string" ? { unit: value.unit } : {}),
  };
}

function parseFields(value: unknown): FieldSpec[] | null {
  if (!Array.isArray(value)) return null;
  const fields = value.map(parseField).filter((f): f is FieldSpec => f !== null);
  return fields.length > 0 ? fields : null;
}

/** Dot-path getter for report items ("timing.after"). */
function getPath(obj: unknown, path: string): unknown {
  let cur: unknown = obj;
  for (const seg of path.split(".")) {
    if (!isRecord(cur)) return undefined;
    cur = cur[seg];
  }
  return cur;
}

function fieldLabel(f: FieldSpec): string {
  return f.label ?? f.field;
}

/** Collapsible raw JSON — the escape hatch, never the default view. */
export function RawData({ value }: { value: unknown }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  return (
    <div className="mt-3">
      <button
        onClick={() => setOpen(!open)}
        className="flex items-center gap-1 text-2xs text-faint active:text-dim"
      >
        <span className={`text-[9px] transition-transform ${open ? "rotate-90" : ""}`}>▶</span>
        {t("zconfig.workflowRawData")}
      </button>
      {open && (
        <pre className="mt-2 max-h-72 overflow-auto rounded-xl bg-raised p-3 font-mono text-2xs leading-relaxed text-dim ring-1 ring-hairline">
          {prettyJson(value)}
        </pre>
      )}
    </div>
  );
}

/** Fallback view: one key-value card per report item (the old metrics look). */
export function KeyValueCards({
  items,
}: {
  items: Array<{ sequence: number; item: unknown }> | null;
}) {
  if (items === null) return null;
  return (
    <div className="flex flex-col gap-2 pt-2">
      {items.map((it) => {
        if (!isRecord(it.item)) {
          return (
            <p
              key={it.sequence}
              className="whitespace-pre-wrap break-all rounded-xl bg-surface px-3 py-2 text-xs text-dim ring-1 ring-hairline"
            >
              {typeof it.item === "string" ? it.item : cellText(it.item)}
            </p>
          );
        }
        return (
          <div key={it.sequence} className="rounded-xl bg-surface px-3 py-2 ring-1 ring-hairline">
            {Object.entries(it.item).map(([k, v]) => (
              <div key={k} className="flex items-baseline justify-between gap-3 py-0.5">
                <span className="min-w-0 shrink-0 truncate text-2xs text-faint">{k}</span>
                <span className="min-w-0 break-all text-right font-mono text-xs text-ink">
                  {cellText(v)}
                </span>
              </div>
            ))}
          </div>
        );
      })}
    </div>
  );
}

/**
 * Table artifacts: columns come from the spec's declared fields, else the
 * first row's own keys. A spec `key` field means "a later item with the same
 * key REPLACES the row" — dedupe newest-wins before rendering.
 */
export function TableView({
  items,
  spec,
}: {
  items: Array<{ sequence: number; item: unknown }> | null;
  spec: unknown;
}) {
  const declared = (() => {
    if (!isRecord(spec)) return null;
    return parseFields(spec.columns);
  })();
  const keyField = isRecord(spec) && typeof spec.key === "string" ? spec.key : null;

  const rows = ((): Array<Record<string, unknown> | null> => {
    const raw = (items ?? []).map((it) => (isRecord(it.item) ? it.item : null));
    if (!keyField) return raw;
    const map = new Map<string, Record<string, unknown>>();
    raw.forEach((r, i) => {
      if (r === null) return;
      const k = getPath(r, keyField);
      map.set(k === undefined || k === null ? `#${i}` : String(k), r);
    });
    return [...map.values()];
  })();

  const firstRow = rows.find((r) => r !== null) ?? null;
  const columns: string[] =
    declared?.map(fieldLabel) ?? (firstRow ? Object.keys(firstRow).slice(0, 8) : []);

  if (items === null || rows.every((r) => r === null)) {
    return (
      <KeyValueCards
        items={(items ?? [])
          .filter((it) => !isRecord(it.item))
          .map((it) => ({ sequence: it.sequence, item: it.item }))}
      />
    );
  }

  return (
    <div className="overflow-x-auto pt-2">
      <table className="w-full border-collapse text-left text-2xs">
        <thead>
          <tr>
            {columns.map((c) => (
              <th
                key={c}
                className="whitespace-nowrap border-b border-hairline px-2 py-1.5 font-medium text-dim"
              >
                {c}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((r, i) => (
            <tr key={i} className="align-top">
              {columns.map((c) => {
                const key = declared?.find((f) => fieldLabel(f) === c)?.field ?? c;
                return (
                  <td
                    key={c}
                    className="max-w-64 truncate border-b border-hairline/50 px-2 py-1.5 text-dim"
                  >
                    {r === null ? cellText(items?.[i]?.item) : cellText(r[key])}
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/**
 * Metrics artifacts with a spec: one tile per declared metric, showing the
 * value from the LATEST item that carries the field (BoardSpec semantics).
 * Without a spec the caller falls back to KeyValueCards.
 */
export function MetricsTiles({
  items,
  spec,
}: {
  items: Array<{ sequence: number; item: unknown }> | null;
  spec: unknown;
}) {
  const fields = isRecord(spec) ? parseFields(spec.metrics) : null;
  if (!fields) return <KeyValueCards items={items} />;
  const latest = new Map<string, unknown>();
  for (const it of items ?? []) {
    if (!isRecord(it.item)) continue;
    for (const f of fields) {
      const v = getPath(it.item, f.field);
      if (v !== undefined && v !== null) latest.set(f.field, v);
    }
  }
  return (
    <div className="grid grid-cols-2 gap-2 pt-2">
      {fields.map((f) => (
        <div key={f.field} className="rounded-xl bg-surface px-3 py-2.5 ring-1 ring-hairline">
          <p className="truncate text-2xs text-faint">{fieldLabel(f)}</p>
          <p className="mt-0.5 truncate font-mono text-base text-ink">
            {cellText(latest.get(f.field)) || "—"}
            {f.unit ? <span className="ml-0.5 text-2xs text-faint">{f.unit}</span> : null}
          </p>
        </div>
      ))}
    </div>
  );
}

interface BoardCard {
  column: string;
  title: string;
  details: Array<{ label: string; value: string }>;
}

/**
 * Board artifacts are a kanban: every report item is a card keyed by the spec
 * `key` field (a later item with the same key UPDATES the card in place),
 * placed in the column named by the `status` field. Mobile stacking: columns
 * render as sections, one under the other, in spec order — unknown statuses
 * trail in a last "other" section.
 */
export function BoardView({
  items,
  spec,
}: {
  items: Array<{ sequence: number; item: unknown }> | null;
  spec: unknown;
}) {
  const { t } = useTranslation();
  const parsed = (() => {
    if (!isRecord(spec)) return null;
    const key = typeof spec.key === "string" ? spec.key : null;
    const status = typeof spec.status === "string" ? spec.status : null;
    const columns = Array.isArray(spec.columns)
      ? spec.columns.filter((c): c is string => typeof c === "string")
      : [];
    if (!key || !status) return null;
    return {
      key,
      status,
      columns,
      cardTitle: typeof spec.cardTitle === "string" ? spec.cardTitle : null,
      details: parseFields(spec.detail),
    };
  })();
  if (!parsed) return <KeyValueCards items={items} />;

  const cards = new Map<string, BoardCard>();
  const extraColumns: string[] = [];
  for (const it of items ?? []) {
    if (!isRecord(it.item)) continue;
    const keyVal = getPath(it.item, parsed.key);
    if (keyVal === undefined || keyVal === null || keyVal === "") continue;
    const key = String(keyVal);
    const colVal = getPath(it.item, parsed.status);
    const column = colVal === undefined || colVal === null ? "—" : String(colVal);
    if (!parsed.columns.includes(column) && !extraColumns.includes(column)) {
      extraColumns.push(column);
    }
    const title =
      parsed.cardTitle !== null
        ? (() => {
            const v = getPath(it.item, parsed.cardTitle);
            return v === undefined || v === null || v === "" ? key : String(v);
          })()
        : key;
    cards.set(key, {
      column,
      title,
      details: (parsed.details ?? [])
        .map((f) => ({
          label: fieldLabel(f),
          value: (() => {
            const v = getPath(it.item, f.field);
            const s = cellText(v);
            return s === "" ? "—" : s;
          })(),
        }))
        .filter((d) => d.value !== "—"),
    });
  }

  const allColumns = [...parsed.columns, ...extraColumns];
  return (
    <div className="flex flex-col gap-4 pt-2">
      {allColumns.map((col) => {
        // Entries, not values: two cards may share a TITLE — the spec key is
        // the only stable identity for React (a card moves columns by update).
        const colCards = [...cards.entries()].filter(([, c]) => c.column === col);
        return (
          <section key={col}>
            <header className="mb-1.5 flex items-baseline gap-2 border-b border-hairline pb-1">
              <span className="text-xs font-medium text-ink">{col}</span>
              <span className="text-2xs text-faint">{colCards.length}</span>
            </header>
            {colCards.length === 0 ? (
              <p className="py-1 text-2xs text-faint">{t("zconfig.workflowBoardEmpty")}</p>
            ) : (
              <div className="flex flex-col gap-1.5">
                {colCards.map(([cardKey, c]) => (
                  <div
                    key={cardKey}
                    className="rounded-xl bg-surface px-3 py-2 ring-1 ring-hairline"
                  >
                    <p className="truncate text-xs text-ink">{c.title}</p>
                    {c.details.map((d) => (
                      <p key={d.label} className="mt-0.5 flex items-baseline justify-between gap-3">
                        <span className="min-w-0 shrink-0 truncate text-2xs text-faint">
                          {d.label}
                        </span>
                        <span className="min-w-0 break-all text-right font-mono text-2xs text-dim">
                          {d.value}
                        </span>
                      </p>
                    ))}
                  </div>
                ))}
              </div>
            )}
          </section>
        );
      })}
    </div>
  );
}

interface ChartPoint {
  x: number;
  y: number;
}

/**
 * Chart artifacts, drawn as a small inline SVG (no chart library on mobile):
 * each tagged report item is one point, x/y read through the spec's dot paths.
 * String x values become category slots by first-seen order. The latest value
 * per series rides under the chart — hover tooltips have no mobile form.
 */
export function ChartView({
  items,
  spec,
}: {
  items: Array<{ sequence: number; item: unknown }> | null;
  spec: unknown;
}) {
  const parsed = (() => {
    if (!isRecord(spec)) return null;
    const x = parseField(spec.x);
    const yField = parseField(spec.y);
    const yArray = parseFields(spec.y);
    const y = yArray ?? (yField !== null ? [yField] : null);
    if (!x || !y) return null;
    const type = spec.type === "bar" || spec.type === "scatter" ? spec.type : "line";
    const baseline = parseField(spec.baseline);
    return {
      x,
      y,
      type,
      scale: spec.scale === "log" ? "log" : "linear",
      baseline: baseline ?? null,
    };
  })();
  if (!parsed) return <KeyValueCards items={items} />;

  const categories: string[] = [];
  /** Numbers and numeric strings both chart (desktop renderer parity). */
  const toNum = (v: unknown): number | null => {
    if (typeof v === "number" && Number.isFinite(v)) return v;
    if (typeof v === "string" && v.trim() !== "") {
      const n = Number(v);
      if (Number.isFinite(n)) return n;
    }
    return null;
  };
  const toX = (item: Record<string, unknown>): number | null => {
    const v = getPath(item, parsed.x.field);
    const n = toNum(v);
    if (n !== null) return n;
    const s = v === undefined || v === null ? null : String(v);
    if (s === null || s === "") return null;
    let idx = categories.indexOf(s);
    if (idx === -1) {
      categories.push(s);
      idx = categories.length - 1;
    }
    return idx;
  };

  const series: Array<{ label: string; points: ChartPoint[]; unit?: string }> = parsed.y.map(
    (f) => {
      const points: ChartPoint[] = [];
      for (const it of items ?? []) {
        if (!isRecord(it.item)) continue;
        const y = toNum(getPath(it.item, f.field));
        if (y === null) continue;
        const x = toX(it.item);
        if (x === null) continue;
        points.push({ x, y });
      }
      // Desktop parity: points order by x — arrival order is journal order.
      points.sort((a, b) => a.x - b.x);
      return { label: fieldLabel(f), points, unit: f.unit };
    },
  );

  const baselinePoint = (() => {
    if (!parsed.baseline) return null;
    for (const it of items ?? []) {
      if (!isRecord(it.item)) continue;
      const v = getPath(it.item, parsed.baseline.field);
      if (typeof v === "number" && Number.isFinite(v)) return v;
    }
    return null;
  })();

  const all = series.flatMap((s) => s.points.map((p) => p.y));
  if (baselinePoint !== null) all.push(baselinePoint);

  const W = 320;
  const H = 180;
  const PAD = { l: 36, r: 10, t: 10, b: 20 };
  const iw = W - PAD.l - PAD.r;
  const ih = H - PAD.t - PAD.b;

  // No points yet: draw the empty frame (axes + legend), not a fallback card.
  let yMin = all.length > 0 ? Math.min(...all) : 0;
  let yMax = all.length > 0 ? Math.max(...all) : 1;
  if (yMin === yMax) {
    yMin -= 1;
    yMax += 1;
  } else {
    const pad = (yMax - yMin) * 0.08;
    yMin -= pad;
    yMax += pad;
  }
  const useLog = parsed.scale === "log" && yMin > 0;
  const tf = (v: number) => (useLog ? Math.log10(v) : v);
  const yPix = (v: number) => PAD.t + ih - ((tf(v) - tf(yMin)) / (tf(yMax) - tf(yMin))) * ih;

  const xs = series.flatMap((s) => s.points.map((p) => p.x));
  const xMin = Math.min(0, ...xs);
  const xMax = Math.max(...xs, categories.length > 0 ? categories.length - 1 : 0);
  const xPix = (v: number) => PAD.l + ((v - xMin) / (xMax - xMin || 1)) * iw;

  // Series colors ride the token palette; SVG attributes cannot hold var()
  // references, so resolve the custom properties (per render, cheap).
  const SERIES_COLORS = [
    "--color-info",
    "--color-success",
    "--color-chart-4",
    "--color-warning",
    "--color-chart-6",
  ].map(
    (v, i) =>
      getComputedStyle(document.documentElement).getPropertyValue(v).trim() ||
      ["#38bdf8", "#34d399", "#f472b6", "#fbbf24", "#a78bfa"][i],
  );
  const fmt = (v: number) =>
    Math.abs(v) >= 1000
      ? v.toLocaleString(undefined, { maximumFractionDigits: 0 })
      : String(Math.round(v * 100) / 100);

  const gridValues = [yMin, (yMin + yMax) / 2, yMax];

  return (
    <div className="pt-2">
      <svg viewBox={`0 0 ${W} ${H}`} className="w-full rounded-xl bg-surface ring-1 ring-hairline">
        {gridValues.map((v, i) => (
          <g key={i}>
            <line
              x1={PAD.l}
              x2={W - PAD.r}
              y1={yPix(v)}
              y2={yPix(v)}
              className="stroke-hairline"
              strokeWidth="1"
            />
            <text
              x={PAD.l - 4}
              y={yPix(v) + 3}
              textAnchor="end"
              fontSize="8"
              className="fill-faint"
            >
              {fmt(v)}
            </text>
          </g>
        ))}
        {baselinePoint !== null && (
          <line
            x1={PAD.l}
            x2={W - PAD.r}
            y1={yPix(baselinePoint)}
            y2={yPix(baselinePoint)}
            className="stroke-faint"
            strokeDasharray="3 3"
            strokeWidth="1"
          />
        )}
        {series.map((s, si) => {
          const color = SERIES_COLORS[si % SERIES_COLORS.length];
          if (parsed.type === "scatter") {
            return (
              <g key={s.label}>
                {s.points.map((p, i) => (
                  <circle key={i} cx={xPix(p.x)} cy={yPix(p.y)} r="2.5" fill={color} />
                ))}
              </g>
            );
          }
          if (parsed.type === "bar") {
            // Bars sit AT their x value (arrival-order slots would misplace
            // numeric x); sibling series share the slot, side by side.
            const n = Math.max(1, ...series.map((s2) => s2.points.length));
            const seriesCount = series.length;
            const barW = Math.max(1.5, (iw / (n * seriesCount)) * 0.7);
            return (
              <g key={s.label}>
                {s.points.map((p, i) => {
                  const cx = xPix(p.x) + (si - (seriesCount - 1) / 2) * barW;
                  const top = yPix(p.y);
                  return (
                    <rect
                      key={i}
                      x={cx - barW / 2}
                      y={top}
                      width={barW}
                      height={Math.max(1, PAD.t + ih - top)}
                      rx="1"
                      fill={color}
                      opacity="0.85"
                    />
                  );
                })}
              </g>
            );
          }
          if (s.points.length === 1) {
            const p = s.points[0]!;
            return <circle key={s.label} cx={xPix(p.x)} cy={yPix(p.y)} r="3" fill={color} />;
          }
          return (
            <polyline
              key={s.label}
              points={s.points.map((p) => `${xPix(p.x)},${yPix(p.y)}`).join(" ")}
              fill="none"
              stroke={color}
              strokeWidth="1.5"
              strokeLinejoin="round"
            />
          );
        })}
        {categories.length > 0 && categories.length <= 12 && (
          <g>
            {categories.map((c, i) => (
              <text
                key={`${i}-${c}`}
                x={xPix(i)}
                y={H - 6}
                textAnchor="middle"
                fontSize="8"
                className="fill-faint"
              >
                {c.length > 6 ? `${c.slice(0, 5)}…` : c}
              </text>
            ))}
          </g>
        )}
      </svg>
      <div className="mt-2 flex flex-col gap-0.5">
        {series.map((s, si) => {
          const last = s.points[s.points.length - 1];
          return (
            <p key={s.label} className="flex items-baseline justify-between gap-3">
              <span className="flex min-w-0 items-center gap-1.5">
                <span
                  className="size-1.5 shrink-0 rounded-full"
                  style={{ background: SERIES_COLORS[si % SERIES_COLORS.length] }}
                />
                <span className="truncate text-2xs text-dim">{s.label}</span>
              </span>
              <span className="shrink-0 font-mono text-2xs text-ink">
                {last ? fmt(last.y) : "—"}
                {s.unit ? <span className="ml-0.5 text-faint"> {s.unit}</span> : null}
              </span>
            </p>
          );
        })}
      </div>
      {useLog && <p className="mt-1 text-2xs text-faint">log scale</p>}
    </div>
  );
}

/**
 * Unknown artifact kinds: a metadata card (title / description / source path /
 * content type) plus any unrecognized top-level fields as key-value rows —
 * the shape is open (`[key: string]: unknown`), so new kinds still read.
 */
export function ArtifactMetaCard({ artifact }: { artifact: WorkflowArtifact }) {
  const { t } = useTranslation();
  const known = new Set([
    "id",
    "kind",
    "title",
    "description",
    "contentType",
    "sourcePath",
    "primary",
    "itemCount",
    "version",
    "versions",
    "spec",
  ]);
  const extras = Object.entries(artifact).filter(([k]) => !known.has(k));
  return (
    <div className="pt-2">
      <span className="inline-block rounded-md bg-hover px-1.5 py-0.5 font-mono text-2xs text-dim">
        {artifact.kind}
      </span>
      {typeof artifact.description === "string" && artifact.description && (
        <p className="mt-2 text-xs leading-relaxed text-dim">{artifact.description}</p>
      )}
      <div className="mt-2 rounded-xl bg-surface px-3 py-2 ring-1 ring-hairline">
        {[
          ["id", artifact.id],
          artifact.contentType !== undefined ? ["type", String(artifact.contentType)] : null,
          typeof artifact.sourcePath === "string"
            ? [t("zconfig.workflowSourcePath"), artifact.sourcePath]
            : null,
          artifact.itemCount !== undefined ? ["items", String(artifact.itemCount)] : null,
        ]
          .filter((x): x is [string, string] => x !== null)
          .map(([k, v]) => (
            <p key={k} className="flex items-baseline justify-between gap-3 py-0.5">
              <span className="shrink-0 text-2xs text-faint">{k}</span>
              <span className="min-w-0 break-all text-right font-mono text-2xs text-dim">
                {v}
              </span>
            </p>
          ))}
        {extras.map(([k, v]) => (
          <p key={k} className="flex items-baseline justify-between gap-3 py-0.5">
            <span className="min-w-0 shrink-0 truncate text-2xs text-faint">{k}</span>
            <span className="min-w-0 break-all text-right font-mono text-2xs text-dim">
              {cellText(v)}
            </span>
          </p>
        ))}
      </div>
    </div>
  );
}
