// Whether a press on the move-mode pill picks it up. Only a primary press
// on the pill itself does: a press on ✓ or ✗ is a click, so it sends no
// drag (which would move the window under the click) and leaves focus alone.
export const startsDrag = (event, moving) =>
  moving && event.button === 0 && !event.target.closest('button');
