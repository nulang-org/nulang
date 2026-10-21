# Resumable output sessions (nulang-session/1)

Status: **transport-neutral reference primitive** in the Nulang runtime, not a
deployed Nulang Cloud feature. No NUL0 protocol change, shell server,
browser connector, durable inbox, persistent session registry, or new language
syntax is provided here.

## Why this belongs above the transport

A TCP connection is not a logical session. A logical session is not a durable
computation. Reconnection must never replay application commands or external
effects merely to reproduce terminal output.

The new src/runtime/resumable_session.rs implements the state machine for a
gateway with an authenticated principal, an unpredictable server-issued
session ID, and an existing process or actor whose output is being streamed.

- Each open session starts at epoch 1, output cursor 0.
- A successful resume increments the epoch and fences the previous connection.
- Output is numbered monotonically within the session, starting at 1.
- A cursor means "last contiguous output frame received by the client." Wire
  cursors are canonical decimal strings (safe across JavaScript hosts).
- Resume/replay returns only frames strictly newer than the client's cursor.
- Retention is bounded per session by both frame count and total output
  bytes. Oversize individual frames are rejected without advancing the cursor.
- If requested data has been evicted, the result is an explicit Gap, not an
  apparently successful replay. The client must resynchronize using an
  authoritative application snapshot or create a fresh session.
- Ack is monotonic and limited to output offered in the current connection
  epoch. It is not a durability or effect-processing acknowledgement.
- Unknown and unauthorized sessions return the same NotFound result.

The wire model is versioned at the Open/Resume handshake. JSON envelopes
have a type discriminator, and unknown protocol versions are rejected.
The wire model defines Open, Resume, Ack, Close and server-side
Ready, Output, Gap, Closed. No handler automatically trusts user-supplied
identities, and no network listener has been attached.

## Security requirements for an actual gateway

1. Authenticate the connection and derive tenant/principal from verified
   claims; never trust those fields in client JSON.
2. Create session IDs with a cryptographically secure RNG. Do not use the
   network address, process-local counter, actor ID, or a guessable token.
3. Reauthorize on every resume and reject revoked accounts/sessions.
4. Serialize state-machine operations per session (actor or mutex), and fence
   stale transports at the point where output is actually emitted.
5. Enforce maximum inbound frame size before deserializing JSON and
   impose tenant/global session-count and memory quotas, idle expiration,
   and session cleanup.
6. Redact credentials from logs; use TLS and configure origin/CSRF checks
   as appropriate for browser transports.
7. Store a session-to-runtime attachment separately from durable actor identity:
   disconnecting a browser must not cancel the attached actor or workflow.

The library enforces local scope comparison and lease epochs; it cannot
authenticate network claims, revoke accounts, or prevent a compromised gateway
from passing false identity values.

## Next implementation stages

1. Implement a Nulang Cloud gateway adapter using WebSocket/TLS with
   authenticated Open, Resume, Ack, and server Output/Gap delivery.
   Validate reconnect after simulated socket loss and concurrent stale client
   attempts.
2. Persist session metadata and a bounded output log backed by existing Fabric
   Streams after identifying its committed/replayable guarantees. The
   in-memory registry intentionally does not claim restart recovery.
3. Add gateway snapshots for replaceable UI state, with an explicit
   generation/revision policy; do not use snapshots to skip durable messages.
4. Measure actual mobile/unstable-network behavior before considering
   QUIC/WebTransport or speculative terminal echo.

Focused test: cargo test --locked --test resumable_sessions
Minimal profile: cargo test --locked --no-default-features --test resumable_sessions
Broader pre-merge validation: cargo test --locked --workspace

The tests exercise selective replay, ownership, stale-epoch fencing,
retention gaps, unsafe future ACKs, size limits, and wire counter fidelity.
