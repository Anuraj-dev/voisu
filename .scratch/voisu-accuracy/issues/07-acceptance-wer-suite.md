# Acceptance: live 4-paragraph suite at ≤10% WER

**Label:** `wayfinder:task` (HITL)  
**Status:** open  
**Blocked by:** 04-impl-groq-accuracy, 05-impl-deepgram-streaming, 06-impl-reconciliation-and-visibility

## Question

Raja re-dictates the same four technical paragraphs live through the rebuilt
pipeline. Score per-source and final WER with the existing scorer. Pass:
overall ≤10% WER, no fluent-nonsense substitutions, both providers present in
history (or their failure visibly recorded). Includes the live Groq model A/B
if the benchmark ticket deferred it.
