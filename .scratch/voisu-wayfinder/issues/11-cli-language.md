# Choose CLI command language

**Label:** `wayfinder:grilling`  
**Status:** closed

## Question

How should Recording commands remain distinct from daemon lifecycle commands?

## Resolution

`voisu start`, `voisu stop`, and `voisu toggle` control a Recording;
`voisu service start|stop|restart|status` controls the daemon; `voisu status`
reports current product state.

