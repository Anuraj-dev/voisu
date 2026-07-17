# Implement Groq accuracy upgrades (prompt, dictionary, model, merge fix)

**Label:** `wayfinder:task` (AFK, Opus subagent)  
**Status:** open  
**Blocked by:** 03-write-prd

## Question

Per the PRD: add Whisper `prompt` (built-in developer vocabulary + user
dictionary merge), `language`, `temperature=0`; make the model configurable
with the benchmark-chosen default; fix the 30 s chunk-seam artifacts (overlap/
merge strategy per PRD). TDD through public seams; no GTK.
