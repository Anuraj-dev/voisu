# Choose the implementation stack

**Label:** `wayfinder:grilling`  
**Status:** closed

## Question

Should Voisu retain the TypeScript/Python stack or use a native stack?

## Resolution

Use Rust for the daemon and GTK4 for the later native Overlay, accepting a
slower start for lower overhead and stronger Linux integration.

