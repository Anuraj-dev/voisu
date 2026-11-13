// `npm test`: the release-note helpers and the bundled notes file.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import notes from './whats-new.json' with { type: 'json' };
import { compareVersions, history, unseen, byKind, itemsFor, KINDS } from './whats-new.js';

const NOTES = [
  { version: '0.10.0', date: '2026-12-01', items: [{ kind: 'new', text: 'Ten' }] },
  { version: '0.9.1', date: '2026-11-20', items: [{ kind: 'fixed', text: 'Mac only', platforms: ['mac'] }] },
  { version: '0.9.0', date: '2026-11-15', items: [{ kind: 'improved', text: 'Nine' }, { kind: 'later', text: 'Unknown kind' }] },
  { version: '0.8.0', date: '2026-11-10', items: [{ kind: 'new', text: 'Eight', platforms: ['windows'] }] },
];

test('versions compare by number, not by text', () => {
  assert.equal(compareVersions('0.10.0', '0.9.0'), 1);
  assert.equal(compareVersions('0.9.0', '0.10.0'), -1);
  assert.equal(compareVersions('v0.9.0', '0.9'), 0);
  assert.equal(compareVersions('1.0.0-beta.1', '1.0.0'), 0);
  assert.equal(compareVersions('0.78.0', '0.77.12'), 1);
});

test('items keep known kinds on their platform', () => {
  const items = [
    { kind: 'new', text: 'both' },
    { kind: 'new', text: 'mac', platforms: ['mac'] },
    { kind: 'removed', text: 'unknown' },
    null,
  ];
  assert.deepEqual(itemsFor(items, 'windows').map((i) => i.text), ['both']);
  assert.deepEqual(itemsFor(items, 'mac').map((i) => i.text), ['both', 'mac']);
  assert.deepEqual(itemsFor('not a list', 'mac'), []);
});

test('history stops at the running version and skips releases with nothing for this platform', () => {
  assert.deepEqual(history(NOTES, '0.9.1', 'windows').map((r) => r.version), ['0.9.0', '0.8.0']);
  assert.deepEqual(history(NOTES, '0.10.0', 'mac').map((r) => r.version), ['0.10.0', '0.9.1', '0.9.0']);
});

test('unseen covers releases after the seen one, up to the running version', () => {
  assert.deepEqual(unseen(NOTES, '0.8.0', '0.10.0', 'windows').map((r) => r.version), ['0.10.0', '0.9.0']);
  assert.deepEqual(unseen(NOTES, '0.9.0', '0.10.0', 'mac').map((r) => r.version), ['0.10.0', '0.9.1']);
});

test('with nothing seen only the running version counts', () => {
  assert.deepEqual(unseen(NOTES, null, '0.10.0', 'windows').map((r) => r.version), ['0.10.0']);
  assert.deepEqual(unseen(NOTES, null, '0.9.1', 'windows'), []);
});

test('nothing is unseen once the running version, or a newer one, was seen', () => {
  assert.deepEqual(unseen(NOTES, '0.10.0', '0.10.0', 'windows'), []);
  assert.deepEqual(unseen(NOTES, '0.11.0', '0.10.0', 'windows'), []);
});

test('byKind groups in New, Improved, Fixed order and drops empty groups', () => {
  const groups = byKind([
    { kind: 'fixed', text: 'a' },
    { kind: 'new', text: 'b' },
    { kind: 'fixed', text: 'c' },
  ]);
  assert.deepEqual(
    groups.map((g) => [g.name, g.items.map((i) => i.text)]),
    [
      ['New', ['b']],
      ['Fixed', ['a', 'c']],
    ],
  );
});

test('the bundled notes are well formed and newest first', () => {
  assert.ok(notes.length > 0);
  const kinds = KINDS.map((k) => k.id);
  notes.forEach((release, i) => {
    assert.match(release.version, /^\d+\.\d+\.\d+$/);
    assert.match(release.date, /^\d{4}-\d{2}-\d{2}$/);
    assert.ok(release.items.length > 0, release.version);
    for (const item of release.items) {
      assert.ok(kinds.includes(item.kind), `${release.version}: ${item.kind}`);
      assert.ok(item.text.trim(), release.version);
      if (item.platforms) assert.ok(item.platforms.every((p) => p === 'windows' || p === 'mac'));
    }
    if (i) assert.equal(compareVersions(notes[i - 1].version, release.version), 1, release.version);
  });
});
