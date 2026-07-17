# Implement Deepgram nova-3 websocket streaming provider

**Label:** `wayfinder:task` (AFK, Opus subagent)  
**Status:** open  
**Blocked by:** 03-write-prd

## Question

Replace the 1-second batch-chunk Deepgram path with real nova-3 websocket
streaming per the guide from ticket 01 and the PRD: live audio framing, final
result assembly, keyterm boosting from the shared dictionary, bounded
error/reconnect, clean abort. TDD through public seams.
