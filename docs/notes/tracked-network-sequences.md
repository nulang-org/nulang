# Tracked NUL0 transport sequences

`NetworkTransport::send` remains the compatibility surface for fire-and-forget callers. `send_tracked` is additive: transports that can reserve a NUL0 sequence synchronously return that sequence, while custom transports may keep the default implementation and return `None` after sending normally.

The built-in TCP and deterministic transports reserve sequence numbers before delivery. A simulated in-flight drop may therefore return `Some(sequence)` without a later ACK; higher-level receiver-knowledge state must advance only after the matching ACK arrives. Wire-unsafe payload rejection returns `None` because no NUL0 sequence is assigned.

This does not change the NUL0 frame format or ACK packet. It only exposes the existing sequence identity to the caller so higher-level protocols can correlate transport acknowledgement with logical state transfer.
