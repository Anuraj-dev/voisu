// The move-mode drag's calls to the native side, one in flight at a time.
// The native side reads the pointer itself, so a call only says "pressed"
// (`start`) or "moved", with the move mode it belongs to (the native side
// ignores one for a move that has ended). `send(start, moveId)` makes the
// call and returns its promise.
//
// What arrives while a call is in flight waits behind it: at most a press
// and one move, since the pointer is read when the move lands. Letting go
// sends one last move, after any still waiting, so the pill ends where it
// was dropped; `idle()` resolves once all of that has landed, so ✓ saves
// that spot.
export function createDrag(send) {
  // The move the held pointer pressed in; null when not held.
  let held = null;
  // The call in flight, and what waits behind it.
  let busy = null;
  let queue = [];

  function call({ start, id }) {
    let sent;
    try {
      sent = Promise.resolve(send(start, id));
    } catch (error) {
      sent = Promise.reject(error);
    }
    busy = sent
      .catch((error) => console.warn('overlay drag failed', error))
      .finally(() => {
        busy = null;
        const next = queue.shift();
        if (next) call(next);
      });
  }

  function push(step) {
    if (step.start) {
      // A new press makes anything still waiting moot.
      queue = [];
    } else {
      const last = queue[queue.length - 1];
      if (last && !last.start && last.id === step.id) return;
    }
    if (busy) queue.push(step);
    else call(step);
  }

  return {
    press(id) {
      held = id;
      push({ start: true, id });
    },
    move() {
      if (held != null) push({ start: false, id: held });
    },
    // Letting go: the last spot still goes.
    release() {
      if (held == null) return;
      push({ start: false, id: held });
      held = null;
    },
    // Move mode ended: nothing more is worth sending.
    cancel() {
      held = null;
      queue = [];
    },
    async idle() {
      while (busy) await busy;
    },
  };
}
