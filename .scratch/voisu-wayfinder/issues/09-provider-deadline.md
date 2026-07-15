# Choose provider completion policy

**Label:** `wayfinder:grilling`  
**Status:** closed

## Question

Should Delivery wait indefinitely for both cloud providers?

## Resolution

Use a configurable Provider Deadline. Reconcile two valid results received in
time; otherwise deliver the valid result already available.

