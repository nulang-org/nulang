# Per-peer CRDT synchronization frontier

The per-peer delta path must distinguish **sent** state from **acknowledged receiver knowledge**.

A sender may compute and retransmit batches from a peer's last acknowledged frontier. Generating a batch must not advance that frontier. Only an acknowledgement for that peer may advance it, and a late acknowledgement for an older batch must never move an already newer frontier backwards.

The legacy global `generate_delta_sync_ops` API remains unchanged during the first compatibility slice; distributed-runtime wiring can migrate to the acknowledged per-peer API separately.
