// The listening cue while a press opens the microphone itself: the flat
// bars at about a third of their brightness, breathing slowly, so the pill
// says "on its way" without looking like it hears anything yet. The live
// bars take over at the first audio.

// Around 35%, never bright enough to read as sound.
export const STARTING_ALPHA = 0.35;
const BREATH_DEPTH = 0.1;
// Slow enough to read as waiting, not as an alert.
const BREATH_PERIOD_MS = 1600;

// The bars' brightness `elapsed` ms into the cue. It starts at its dimmest
// and rises, so the pill fades in; under reduced motion it holds still.
export function startingAlpha(elapsed, reduced) {
  if (reduced) return STARTING_ALPHA;
  const phase = (2 * Math.PI * Math.max(0, elapsed)) / BREATH_PERIOD_MS;
  return STARTING_ALPHA - BREATH_DEPTH * Math.cos(phase);
}
