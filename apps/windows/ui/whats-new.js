// Pure helpers for the release notes in whats-new.json and the pending
// update's notes. No imports, so `node --test` can run them as they are.

export const KINDS = [
  { id: 'new', name: 'New' },
  { id: 'improved', name: 'Improved' },
  { id: 'fixed', name: 'Fixed' },
];

// Numeric x.y.z comparison: '0.10.0' is newer than '0.9.0'. A leading v and
// any pre-release or build suffix are ignored; a missing part counts as 0.
export function compareVersions(a, b) {
  const parts = (v) =>
    String(v ?? '')
      .trim()
      .replace(/^v/i, '')
      .split(/[-+]/)[0]
      .split('.')
      .map((n) => Number.parseInt(n, 10) || 0);
  const x = parts(a);
  const y = parts(b);
  for (let i = 0; i < Math.max(x.length, y.length, 3); i++) {
    const d = (x[i] ?? 0) - (y[i] ?? 0);
    if (d) return Math.sign(d);
  }
  return 0;
}

// An item without `platforms` applies to both; unknown kinds are dropped so
// a later release's kind never shows unlabelled.
export function itemsFor(items, platform) {
  if (!Array.isArray(items)) return [];
  return items.filter(
    (item) =>
      item &&
      typeof item.text === 'string' &&
      KINDS.some((k) => k.id === item.kind) &&
      (!Array.isArray(item.platforms) || item.platforms.includes(platform)),
  );
}

// The releases up to the running version that have something for this
// platform, newest first.
export function history(notes, current, platform) {
  return notes
    .filter((release) => !current || compareVersions(release.version, current) <= 0)
    .map((release) => ({ ...release, items: itemsFor(release.items, platform) }))
    .filter((release) => release.items.length)
    .sort((a, b) => compareVersions(b.version, a.version));
}

// The releases the user has not been shown yet: those newer than `seen`, up
// to the running version. With nothing seen (the first launch of a version
// that tracks it) only the running version's own notes count.
export function unseen(notes, seen, current, platform) {
  if (!current || (seen && compareVersions(seen, current) >= 0)) return [];
  return history(notes, current, platform).filter((release) =>
    seen ? compareVersions(release.version, seen) > 0 : compareVersions(release.version, current) === 0,
  );
}

// Items of several releases, grouped by kind in KINDS order; empty groups
// are left out.
export function byKind(items) {
  return KINDS.map((kind) => ({ ...kind, items: items.filter((item) => item.kind === kind.id) })).filter(
    (group) => group.items.length,
  );
}
