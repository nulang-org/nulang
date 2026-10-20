# Local monitor DOWN flush (experimental)

This change is stacked on the correlated-monitor references introduced in
[#1754](https://github.com/nulang-org/nulang/pull/1754). It is a **Rust runtime
API**, not a language builtin or a distributed monitor protocol.

`Runtime::demonitor_ref_flush(watcher, reference) -> Result<usize, &'static str>`
cancels the watch if it is still registered and removes queued reference-bearing
DOWN notifications delivered to that watcher's local mailbox. The return value
is the count of matching DOWN messages removed. Zero is expected if the target
has not exited yet, the message was already consumed, or the watch was canceled.

Only system-priority, behavior-id-zero, four-value DOWN envelopes whose
embedded watcher and reference match are eligible for removal. Legacy
three-value DOWN messages, other references, and unrelated application messages
are preserved.

**Selective receive:** flushing an active receive candidate or a guard-rejected
candidate that has not been reset is rejected without modification. Call the
operation outside an active receive transaction and retry after it finishes.
For eligible mailboxes, the method stages all queued arrivals using the
existing selective-receive machinery, removes the matching envelopes, releases
their logical mailbox slots, and invalidates receive indexes. The scan is
O(number of pending messages) and is not appropriate for a tight hot path.

**Scope:** local runtime, local watcher, local monitor registration only. This
does not revoke external reply aliases, cancel already executing handlers,
remove remote network packets, guarantee durable delivery, or implement
Erlang's distributed demonitor semantics. Monitor references are process-local.

Focused verification (once a Rust toolchain is available):

```sh
cargo fmt --all -- --check
cargo test --locked --lib monitor_ref_flush_
cargo test --locked --lib monitor_reference
cargo test --locked
```

The tests cover a DOWN already queued after target exit, DOWN messages staged by
selective receive, unrelated references, a live monitor canceled before exit,
and refusal to mutate an active selective-receive transaction.
