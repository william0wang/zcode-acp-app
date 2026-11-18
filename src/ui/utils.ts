// Class-name joiner for the primitives layer: filters falsy, joins with
// spaces. The whole design system routes styling decisions through here so
// call sites stay declarative.
export function cn(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(" ");
}
