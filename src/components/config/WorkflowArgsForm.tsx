import { useEffect, useState } from "react";
import { argsDeclaration } from "../../lib/workflow-args";
import type { WorkflowArgDeclaration } from "../../lib/types";

export interface ArgsFormState {
  args?: Record<string, unknown>;
  error?: string;
}

/**
 * Declaration-driven launch-args form (the desktop's typed form, mobile
 * edition). Fields render by declared type — string input, number input,
 * boolean checkbox; anything else falls to a per-field JSON textarea.
 *
 * Every change reports `{args, error}` upward: `error` covers a malformed
 * JSON field or a missing required field, so the host sheet only disables
 * submit on it. Unset optional fields are simply absent from `args` — the
 * backend applies declared defaults itself.
 */
export function WorkflowArgsForm({
  meta,
  onChange,
}: {
  meta: Record<string, unknown> | null | undefined;
  onChange: (next: ArgsFormState) => void;
}) {
  const declaration = argsDeclaration(meta);
  // Typed field values (string/number/boolean) and the RAW text of JSON
  // fields live in component state; the parsed args are derived per change.
  const [fieldValues, setFieldValues] = useState<Record<string, unknown>>({});
  const [jsonText, setJsonText] = useState<Record<string, string>>({});

  if (declaration === null) return null;
  // Narrowed alias: TS does not carry the early-return narrowing into the
  // function closures below.
  const decls = declaration;

  function report(
    values: Record<string, unknown>,
    json: Record<string, string>,
  ): ArgsFormState {
    const args: Record<string, unknown> = {};
    let error: string | undefined;
    for (const [name, decl] of Object.entries(decls)) {
      const type = decl.type ?? "string";
      const value = values[name];
      if (type === "number") {
        if (typeof value === "number" && Number.isFinite(value))
          args[name] = value;
        else if (value !== undefined && value !== "")
          error = `${name}: invalid number`;
      } else if (type === "boolean") {
        // Booleans always carry a value (desktop contract): unchecked = false.
        args[name] = value === true;
      } else if (type === "string") {
        const s = typeof value === "string" ? value.trim() : "";
        if (s) args[name] = s;
      } else {
        const text = (json[name] ?? "").trim();
        if (text) {
          try {
            args[name] = JSON.parse(text) as unknown;
          } catch {
            error = `${name}: invalid JSON`;
          }
        }
      }
      // Required only when the backend has no declared default to fall back
      // on — a required+default field may stay empty; the server fills it.
      if (
        error === undefined &&
        decl.required &&
        decl.default === undefined &&
        args[name] === undefined
      ) {
        error = `${name} is required`;
      }
    }
    return error ? { error } : { args };
  }

  // Mount report: booleans prefill their declared default, and a missing
  // required field must gate the submit BEFORE the first interaction.
  useEffect(() => {
    const initial: Record<string, unknown> = {};
    for (const [name, decl] of Object.entries(decls)) {
      if ((decl.type ?? "string") === "boolean" && decl.default === true)
        initial[name] = true;
    }
    if (Object.keys(initial).length > 0) setFieldValues(initial);
    onChange(report(initial, {}));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function updateField(name: string, value: unknown) {
    const nextValues = { ...fieldValues, [name]: value };
    setFieldValues(nextValues);
    onChange(report(nextValues, jsonText));
  }

  function updateJson(name: string, text: string) {
    const nextJson = { ...jsonText, [name]: text };
    setJsonText(nextJson);
    onChange(report(fieldValues, nextJson));
  }

  const inputClass =
    "w-full rounded-xl bg-raised p-3 text-sm text-ink ring-1 ring-inset ring-hairline placeholder:text-faint focus:outline-none";

  function fieldLabel(name: string, decl: WorkflowArgDeclaration) {
    return (
      <span className="block text-xs text-dim">
        {name}
        {decl.required ? " *" : ""}
        {decl.description ? (
          <span className="block text-2xs text-faint">
            {decl.description}
          </span>
        ) : null}
      </span>
    );
  }

  return (
    <div className="flex flex-col gap-3">
      {Object.entries(declaration).map(([name, decl]) => {
        const type = decl.type ?? "string";
        if (type === "boolean") {
          return (
            <label
              key={name}
              className="flex items-center gap-2.5 rounded-xl bg-raised px-3 py-3 ring-1 ring-inset ring-hairline"
            >
              <input
                type="checkbox"
                checked={fieldValues[name] === true}
                onChange={(e) => updateField(name, e.target.checked)}
                className="size-4 accent-white"
              />
              {fieldLabel(name, decl)}
            </label>
          );
        }
        if (type === "number") {
          return (
            <div key={name}>
              {fieldLabel(name, decl)}
              <input
                value={
                  typeof fieldValues[name] === "number" &&
                  Number.isFinite(fieldValues[name])
                    ? String(fieldValues[name])
                    : ""
                }
                onChange={(e) =>
                  updateField(
                    name,
                    e.target.value === "" ? "" : Number(e.target.value),
                  )
                }
                inputMode="decimal"
                className={`${inputClass} mt-1`}
              />
            </div>
          );
        }
        if (type === "string") {
          return (
            <div key={name}>
              {fieldLabel(name, decl)}
              <input
                value={
                  typeof fieldValues[name] === "string" ? fieldValues[name] : ""
                }
                onChange={(e) => updateField(name, e.target.value)}
                className={`${inputClass} mt-1`}
              />
            </div>
          );
        }
        return (
          <div key={name}>
            {fieldLabel(name, decl)}
            <textarea
              value={jsonText[name] ?? ""}
              onChange={(e) => updateJson(name, e.target.value)}
              rows={3}
              spellCheck={false}
              className={`${inputClass} mt-1 font-mono`}
            />
          </div>
        );
      })}
    </div>
  );
}
