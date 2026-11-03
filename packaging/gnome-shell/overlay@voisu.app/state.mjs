export const MAX_RESPONSE_BYTES = 64 * 1024;

export class RequestLifecycle {
  constructor() {
    this._generation = 0;
    this._requests = new Set();
  }

  enable() {
    return ++this._generation;
  }

  register(generation, cancel) {
    const request = {generation, cancel};
    if (generation !== this._generation) {
      cancel();
      return request;
    }
    this._requests.add(request);
    return request;
  }

  finish(request) {
    this._requests.delete(request);
    return request.generation === this._generation;
  }

  disable() {
    this._generation++;
    const requests = [...this._requests];
    this._requests.clear();
    for (const request of requests)
      request.cancel();
  }
}

export function parseBoundedResponse(line) {
  if (typeof line !== 'string' || new TextEncoder().encode(line).length > MAX_RESPONSE_BYTES)
    throw new Error('invalid daemon response size');
  const response = JSON.parse(line);
  if (!response || typeof response !== 'object' || response.version !== 1 || typeof response.ok !== 'boolean')
    throw new Error('invalid daemon response');
  return response;
}

// How long a terminal capsule stays up. The pasted check mark only needs to
// register; anything that asks the user to read or act holds longer.
export const SUCCESS_HOLD_MS = 600;
export const TERMINAL_HOLD_MS = 1400;

// The daemon retains a Recording's event the moment Delivery lands, while it
// still reports Processing. Only an event naming that same Recording may
// replace the Processing capsule; an earlier Recording's event may not.
function eventForCurrentRecording(response, event) {
  const correlation = response.evidence?.correlation_id;
  return Boolean(event && correlation && event.correlation_id === correlation);
}

export function capsuleForResponse(response, lastEvent = null) {
  if (!response.ok)
    return {visible: true, text: response.message || 'Voisu rejected request', styleClass: 'voisu-failure', terminal: true, holdMs: TERMINAL_HOLD_MS};
  if (response.state === 'recording')
    return {visible: true, text: 'Recording', styleClass: 'voisu-recording', terminal: false};
  const event = response.overlay_event;
  if (response.state === 'processing' && !eventForCurrentRecording(response, event))
    return {visible: true, text: 'Processing', styleClass: 'voisu-processing', terminal: false};
  if (!event) return {visible: false};
  const eventIdentity = `${event.instance}:${event.id}`;
  if (eventIdentity === lastEvent) return {visible: false};
  if (['empty_recording', 'too_short_recording', 'silent_recording'].includes(event.outcome))
    return {visible: true, text: 'No speech', styleClass: 'voisu-nospeech', terminal: true, holdMs: TERMINAL_HOLD_MS, eventIdentity};
  // Direct Delivery fell back to the clipboard: calm, not a failure, and the
  // text says what to press.
  if (event.outcome === 'copied')
    return {visible: true, text: 'Copied · Ctrl+V', styleClass: 'voisu-copied', terminal: true, holdMs: TERMINAL_HOLD_MS, eventIdentity};
  const success = event.outcome === 'delivered';
  return {
    visible: true,
    text: success ? '✓' : 'Recording failed',
    styleClass: success ? 'voisu-success' : 'voisu-failure',
    terminal: true,
    holdMs: success ? SUCCESS_HOLD_MS : TERMINAL_HOLD_MS,
    eventIdentity,
  };
}
