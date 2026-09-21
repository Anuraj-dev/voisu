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

export function capsuleForResponse(response, lastEvent = null) {
  if (!response.ok)
    return {visible: true, text: response.message || 'Voisu rejected request', styleClass: 'voisu-failure', terminal: true};
  if (response.state === 'recording')
    return {visible: true, text: 'Recording', styleClass: 'voisu-recording', terminal: false};
  if (response.state === 'processing')
    return {visible: true, text: 'Processing', styleClass: 'voisu-processing', terminal: false};
  const event = response.overlay_event;
  if (!event) return {visible: false};
  const eventIdentity = `${event.instance}:${event.id}`;
  if (eventIdentity === lastEvent) return {visible: false};
  if (['empty_recording', 'too_short_recording', 'silent_recording'].includes(event.outcome))
    return {visible: true, text: 'No speech', styleClass: 'voisu-nospeech', terminal: true, eventIdentity};
  const success = event.outcome === 'delivered';
  return {
    visible: true,
    text: success ? '✓' : 'Recording failed',
    styleClass: success ? 'voisu-success' : 'voisu-failure',
    terminal: true,
    eventIdentity,
  };
}
