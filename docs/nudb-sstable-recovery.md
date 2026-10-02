# NuDB SSTable recovery

This slice promotes manifest-referenced SSTables into tablet reopen recovery while keeping WAL append semantics and WAL reclamation policy unchanged.

- Reopen loads `NUDBMAN1`, opens every referenced `NUDBSST1`, and verifies the file metadata exactly matches the manifest entry.
- Missing, corrupt, or metadata-mismatched manifest files fail closed before serving.
- A checkpoint remains the oldest recovery baseline when present. SSTable versions already covered by that checkpoint are skipped; newer SSTable versions are restored as immutable generations.
- A recovered SSTable advances the predecessor only when its row versions prove contiguous sequence coverage above the current recovery floor. Gapped tables are skipped so checkpoint/WAL must supply the missing chain.
- Once contiguous coverage is proven, the reconstructed SSTable sequence becomes the predecessor for subsequent WAL replay, so a WAL whose base sequence has already advanced can reopen without requiring an otherwise redundant checkpoint.
- WAL records newer than the SSTable baseline continue replaying normally.
- SSTable files are still decoded into in-memory immutable generations on reopen; direct block-backed reads and immutable-memory eviction are intentionally deferred.
- WAL reclamation remains conservative. No new reclamation is performed merely because an SSTable exists.

The next slice should introduce direct SSTable-backed read serving (or a block cache), then evict flushed immutable generations only after the same manifest/SSTable validation path is active in-process. After that, WAL segment reclamation can be tied to proven durable sequence coverage.
