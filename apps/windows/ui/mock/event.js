// Mock of @tauri-apps/api/event for `npm run dev:mock`.
const handlers = new Map();

export function listen(name, handler) {
  if (!handlers.has(name)) handlers.set(name, new Set());
  handlers.get(name).add(handler);
  return Promise.resolve(() => handlers.get(name).delete(handler));
}

export function emit(name, payload) {
  for (const handler of handlers.get(name) ?? []) handler({ event: name, payload });
}

export const hasListener = (name) => (handlers.get(name)?.size ?? 0) > 0;
