// Run: node --test ui/overlay/drag.test.js
import test from 'node:test';
import assert from 'node:assert/strict';

import { createDrag } from './drag.js';

// A native side that answers each call only when the test says so.
function fakeNative() {
  const calls = [];
  const answers = [];
  const send = (start, moveId) => {
    calls.push({ start, moveId });
    return new Promise((resolve) => answers.push(resolve));
  };
  // Answers the oldest unanswered call, then lets the queue run.
  const answer = async () => {
    answers.shift()();
    await new Promise((resolve) => setImmediate(resolve));
  };
  return { calls, send, answer };
}

const settled = async (promise) => {
  let done = false;
  promise.then(() => (done = true));
  await new Promise((resolve) => setImmediate(resolve));
  return done;
};

test('a move waiting behind a call still lands after the pointer is let go', async () => {
  const native = fakeNative();
  const drag = createDrag(native.send);
  drag.press(7);
  drag.move();
  drag.move();
  drag.release();
  drag.release(); // lostpointercapture follows pointerup
  assert.deepEqual(native.calls, [{ start: true, moveId: 7 }]);

  const idle = drag.idle();
  assert.equal(await settled(idle), false, 'the press is still in flight');
  await native.answer();
  // The moves and the release's last spot go as one move, after the press.
  assert.deepEqual(native.calls, [
    { start: true, moveId: 7 },
    { start: false, moveId: 7 },
  ]);
  assert.equal(await settled(idle), false, 'the last move has not landed');
  await native.answer();
  assert.equal(await settled(idle), true);
  assert.equal(native.calls.length, 2);
});

test('letting go with nothing in flight sends the last spot at once', async () => {
  const native = fakeNative();
  const drag = createDrag(native.send);
  drag.press(3);
  await native.answer();
  drag.release();
  assert.deepEqual(native.calls.at(-1), { start: false, moveId: 3 });
  await native.answer();
  assert.equal(await settled(drag.idle()), true);
});

test('a press waiting behind a call goes as a press, then its moves', async () => {
  const native = fakeNative();
  const drag = createDrag(native.send);
  drag.press(1);
  drag.release();
  drag.press(1);
  drag.move();
  await native.answer();
  await native.answer();
  assert.deepEqual(native.calls, [
    { start: true, moveId: 1 },
    { start: true, moveId: 1 },
    { start: false, moveId: 1 },
  ]);
});

test('a move with no press, or after move mode ended, sends nothing', async () => {
  const native = fakeNative();
  const drag = createDrag(native.send);
  drag.move();
  drag.release();
  assert.equal(native.calls.length, 0);

  drag.press(2);
  drag.move();
  drag.cancel();
  drag.move();
  drag.release();
  await native.answer();
  assert.deepEqual(native.calls, [{ start: true, moveId: 2 }]);
  assert.equal(await settled(drag.idle()), true);
});
