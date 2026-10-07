# Tracked NUL0 transport sequences

`NetworkTransport::send` remains the compatibility surface for fire-and-forget callers. `send_tracked` is additive and returns an explicit `TrackedSendOutcome`:

- `Sent(sequence)` — the transport assigned an exact NUL0 sequence. Higher-level receiver knowledge may advance only after the matching ACK.
- `Unsupported` — the transport cannot expose tracked sequence identity. Higher-level callers may explicitly use the legacy `send` fallback.
- `Rejected` — the transport understood tracked sending but rejected the packet before assigning a sequence. Callers must not reinterpret this as an untracked successful send.

The default custom-transport implementation returns `Unsupported` **without sending**, so compatibility callers can fall back exactly once without risking a duplicate transmission.

The built-in TCP and deterministic transports reserve sequence numbers before delivery. A simulated in-flight drop therefore returns `Sent(sequence)` without a later ACK; higher-level receiver-knowledge state must stay at the last acknowledged frontier and retry from there. Wire-unsafe TCP payload rejection returns `Rejected`, which leaves the receiver frontier unchanged.

This does not change the NUL0 frame format or ACK packet. It only makes the transport outcome unambiguous so higher-level protocols can distinguish unsupported tracking from an actual tracked-send rejection.
