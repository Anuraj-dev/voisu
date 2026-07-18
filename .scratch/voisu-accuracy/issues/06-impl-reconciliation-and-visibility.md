# Implement reconciliation source-quality gating and provider-failure visibility

**Label:** `wayfinder:task` (AFK, Opus subagent)  
**Status:** open  
**Blocked by:** 03-write-prd

## Question

Per the PRD: discard catastrophically divergent source Transcripts instead of
LLM-merging them; record provider failures/absence in history records
(provider, stage, diagnostic) so a missing source is never silent. TDD through
public seams.
