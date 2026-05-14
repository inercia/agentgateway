---
name: check-patches
description: Verify Adobe-specific JWT (IMS) and SSE session header patches are still present after sync or conflict resolution.
---

# Check Adobe patches (post-sync)

Run these checks from the **repository root** of Adobe-Apis/agentgateway.

## JWT (`crates/agentgateway/src/jwt.rs`)

IMS-oriented token validation differs from upstream. Spot-check that Adobe semantics are not accidentally reverted:

```bash
grep -nE 'claim_as_millis|TokenError::Expired|expires_in|created_at|jsonwebtoken' crates/agentgateway/src/jwt.rs
```

Engineers should confirm that Adobe’s handling around **exp/created_at** and **TokenError::Expired** still matches Ethos expectations (upstream sometimes removes or reshapes `exp` handling).

## SSE (`crates/agentgateway/src/sse.rs`)

Adobe adds a response header for MCP SSE sessions so Envoy can correlate streams:

```bash
grep -nE 'HEADER_SESSION_ID|mcp-session-id' crates/agentgateway/src/sse.rs
```

## Adobe tree

```bash
test -d adobe && grep -RInE 'adobe|CONFIG|CONVENTIONS' adobe | head -n 50
```

## OUTPUT (copy/paste block)

```
## check-patches results
- [ ] jwt.rs greps reviewed — OK / issues: ___ 
- [ ] sse.rs HEADER_SESSION_ID / mcp-session-id present — OK / issues: ___ 
- [ ] adobe/ tree sanity spot-check — OK / issues: ___ 

Notes:
```

Do **not** treat absence of matches as automatically safe — compare against the expected Adobe diff when in doubt.
