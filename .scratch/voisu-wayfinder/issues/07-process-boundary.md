# Choose daemon and Overlay boundaries

**Label:** `wayfinder:grilling`  
**Status:** closed

## Question

Should the reliable daemon and GTK4 Overlay share a process?

## Resolution

Use separate processes with versioned Unix IPC. Finish and verify the daemon
before implementing the Overlay.

