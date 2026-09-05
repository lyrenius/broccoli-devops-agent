/** Joins class names, skipping falsy entries. Small enough that tailwind-merge is not needed. */
export function cn(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(" ");
}
