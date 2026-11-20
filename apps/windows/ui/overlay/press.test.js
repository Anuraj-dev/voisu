// Run: node --test ui/overlay/press.test.js
import test from 'node:test';
import assert from 'node:assert/strict';

import { startsDrag } from './press.js';

// A press event on the move pill: `inButton` says whether its target sits in
// ✓ or ✗ (what `closest('button')` finds in the real page).
const press = ({ button = 0, inButton = false } = {}) => ({
  button,
  target: { closest: (selector) => (selector === 'button' && inButton ? {} : null) },
});

test('a primary press on the pill picks it up in move mode', () => {
  assert.equal(startsDrag(press(), true), true);
});

test('a press on the check or cross is a click and starts no drag', () => {
  assert.equal(startsDrag(press({ inButton: true }), true), false);
});

test('other buttons and presses outside move mode start no drag', () => {
  assert.equal(startsDrag(press({ button: 2 }), true), false);
  assert.equal(startsDrag(press({ button: 1 }), true), false);
  assert.equal(startsDrag(press(), false), false);
});
