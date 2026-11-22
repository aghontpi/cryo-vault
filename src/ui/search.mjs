/** Find literal, case-insensitive matches using offsets in the original text. */
export function findMatches(text, term) {
  if (!term) return [];

  const escapedTerm = term.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const pattern = new RegExp(escapedTerm, "giu");
  return Array.from(text.matchAll(pattern), (match) => ({
    start: match.index,
    end: match.index + match[0].length,
  }));
}
