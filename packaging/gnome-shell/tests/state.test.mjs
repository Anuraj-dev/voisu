import assert from 'node:assert/strict';
import test from 'node:test';
import {
  MAX_RESPONSE_BYTES, RequestLifecycle, SUCCESS_HOLD_MS, TERMINAL_HOLD_MS, capsuleForResponse, parseBoundedResponse,
} from '../overlay@voisu.app/state.mjs';

test('daemon state maps to the native capsule phases', () => {
  assert.deepEqual(capsuleForResponse({ok: true, state: 'recording'}), {
    visible: true, text: 'Recording', styleClass: 'voisu-recording', terminal: false,
  });
  assert.deepEqual(capsuleForResponse({ok: true, state: 'processing'}), {
    visible: true, text: 'Processing', styleClass: 'voisu-processing', terminal: false,
  });
  assert.equal(capsuleForResponse({ok: true, state: 'idle'}).visible, false);
});

test('terminal events map to success, no-speech, and failure once', () => {
  const success = capsuleForResponse({ok: true, overlay_event: {instance: 4, id: 2, outcome: 'delivered'}});
  assert.equal(success.styleClass, 'voisu-success');
  assert.equal(success.eventIdentity, '4:2');
  assert.equal(capsuleForResponse({ok: true, overlay_event: {instance: 4, id: 2, outcome: 'delivered'}}, '4:2').visible, false);
  assert.equal(capsuleForResponse({ok: true, overlay_event: {instance: 4, id: 3, outcome: 'silent_recording'}}).styleClass, 'voisu-nospeech');
  assert.equal(capsuleForResponse({ok: true, overlay_event: {instance: 4, id: 4, outcome: 'provider_failure'}}).styleClass, 'voisu-failure');
});

test('the pasted check mark clears faster than capsules the user must read', () => {
  assert.equal(SUCCESS_HOLD_MS, 600);
  assert.equal(capsuleForResponse({ok: true, overlay_event: {instance: 4, id: 2, outcome: 'delivered'}}).holdMs, SUCCESS_HOLD_MS);
  for (const outcome of ['provider_failure', 'silent_recording', 'copied'])
    assert.equal(capsuleForResponse({ok: true, overlay_event: {instance: 4, id: 3, outcome}}).holdMs, TERMINAL_HOLD_MS, outcome);
  assert.equal(capsuleForResponse({ok: false, message: 'Voisu rejected request'}).holdMs, TERMINAL_HOLD_MS);
});

test('a clipboard-only Delivery is a calm Copied capsule, not a failure', () => {
  const copied = capsuleForResponse({ok: true, overlay_event: {instance: 4, id: 5, outcome: 'copied'}});
  assert.equal(copied.text, 'Copied · Ctrl+V');
  assert.equal(copied.styleClass, 'voisu-copied');
  assert.equal(copied.terminal, true);
});

test('an event for the Recording still processing confirms at once; an earlier one does not', () => {
  const processing = (event) => ({ok: true, state: 'processing', evidence: {correlation_id: 'rec-b'}, overlay_event: event});
  const current = {instance: 4, id: 6, outcome: 'delivered', correlation_id: 'rec-b'};
  const early = capsuleForResponse(processing(current));
  assert.equal(early.styleClass, 'voisu-success');
  assert.equal(early.eventIdentity, '4:6');
  // Shown once: the next Processing poll leaves it to its hold timer.
  assert.equal(capsuleForResponse(processing(current), '4:6').visible, false);
  for (const stale of [{instance: 4, id: 5, outcome: 'delivered', correlation_id: 'rec-a'}, {instance: 4, id: 5, outcome: 'delivered'}])
    assert.equal(capsuleForResponse(processing(stale)).styleClass, 'voisu-processing');
});

test('bounded parser rejects oversized and malformed daemon replies', () => {
  assert.deepEqual(parseBoundedResponse('{"version":1,"ok":true,"state":"idle"}').state, 'idle');
  assert.throws(() => parseBoundedResponse('x'.repeat(MAX_RESPONSE_BYTES + 1)), /size/);
  assert.throws(() => parseBoundedResponse('{"version":2,"ok":true}'), /invalid/);
});

test('rejected Toggle response renders the daemon reason', () => {
  const capsule = capsuleForResponse({ok: false, message: 'Recording is already processing'});
  assert.equal(capsule.styleClass, 'voisu-failure');
  assert.equal(capsule.text, 'Recording is already processing');
});

test('disable cancels old requests and isolates a re-enabled generation', () => {
  const lifecycle = new RequestLifecycle();
  const firstGeneration = lifecycle.enable();
  let cancelled = 0;
  const stale = lifecycle.register(firstGeneration, () => cancelled++);

  lifecycle.disable();
  const secondGeneration = lifecycle.enable();
  const current = lifecycle.register(secondGeneration, () => cancelled++);

  assert.equal(cancelled, 1);
  assert.equal(lifecycle.finish(stale), false);
  assert.equal(lifecycle.finish(current), true);
});
