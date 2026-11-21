// Run: node --test ui/overlay/starting.test.js
import test from 'node:test';
import assert from 'node:assert/strict';

import { STARTING_ALPHA, startingAlpha } from './starting.js';

test('the cue holds still at about 35% under reduced motion', () => {
  for (const elapsed of [0, 400, 800, 5000]) {
    assert.equal(startingAlpha(elapsed, true), STARTING_ALPHA);
  }
});

test('the cue breathes around 35% and stays dim', () => {
  const samples = Array.from({ length: 64 }, (_, i) => startingAlpha(i * 50, false));
  const min = Math.min(...samples);
  const max = Math.max(...samples);
  assert.ok(max > min + 0.1, 'it moves');
  assert.ok(min >= 0.2 && max <= 0.5, `stays dim: ${min}..${max}`);
  const mean = samples.reduce((sum, a) => sum + a, 0) / samples.length;
  assert.ok(Math.abs(mean - STARTING_ALPHA) < 0.02, `mean ${mean}`);
});

test('the cue fades in from its dimmest', () => {
  assert.ok(startingAlpha(0, false) < startingAlpha(400, false));
  assert.equal(startingAlpha(0, false), Math.min(startingAlpha(0, false), startingAlpha(800, false)));
});
