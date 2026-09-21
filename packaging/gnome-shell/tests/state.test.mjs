import assert from 'node:assert/strict';
import test from 'node:test';
import {MAX_RESPONSE_BYTES, RequestLifecycle, capsuleForResponse, parseBoundedResponse} from '../overlay@voisu.app/state.mjs';

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
