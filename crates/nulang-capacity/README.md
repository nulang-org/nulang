# nulang-capacity

Provider-neutral capacity scoring and placement primitives for Nulang Cloud.

This crate is deliberately separated from the Nulang language/runtime and from cloud SDKs. Provider integrations translate native inventory, pricing, interruption, and availability signals into `CapacityOffer`; the broker applies hard constraints and ranks the remaining offers by expected cost to complete the job.

## Design rules

1. **Providers normalize; they do not schedule.** AWS, GCP, Nebius, CoreWeave, RunPod, Vast, and future providers emit the same `CapacityOffer` model.
2. **Hard constraints run before scoring.** Architecture, CPU, memory, region, trust, interruptibility, GPU model/count, and VRAM are eligibility requirements rather than soft preferences.
3. **Optimize completed-job economics.** Ranking includes observed throughput, interruption recovery, startup latency, acquisition confidence, storage, and egress rather than comparing hourly price alone.
4. **Interruptibility is explicit policy.** Critical workloads can reject Spot/preemptible capacity entirely; checkpointable and ephemeral workloads can opt in.
5. **Provider failures are isolated.** The broker continues ranking healthy providers when one provider endpoint fails.
6. **Stale capacity does not compete.** Production callers can reject provider snapshots older than a configured maximum age before ranking.
7. **Provider health stays explicit.** Caller-owned circuit-breaker state handles retryable failures with bounded backoff and non-retryable failures with a long cooldown; the scheduler itself has no hidden global state.
8. **Placement is idempotent.** A durable compare-and-set job claim prevents scheduler replicas from racing one job across providers. Replays using the same placement token may resume the existing claim, while other tokens are rejected.
9. **Provider acquisition is idempotent too.** Offer-scoped, length-prefixed idempotency material is stable across retries; adapters may hash it to satisfy provider-native token limits.
10. **Ambiguous acquisition never falls through.** If a provider may have allocated capacity but its response was lost, the placement remains claimed and must be reconciled before another provider is tried.
11. **Telemetry replaces static assumptions.** Observed fleet data can update throughput, interruption probability, startup percentiles, and acquisition confidence before scoring.
12. **Concrete topology is separate from provider offers.** Resource providers model allocatable hosts/devices with integer inventories, traits, parent relationships, and explicit host/rack/zone/region/provider failure domains.
13. **Candidate generation precedes scoring.** Topology eligibility is a hard-constraint phase; economics and locality may rank only candidates that can actually satisfy the request.
14. **Replica placement is deterministic and topology-aware.** A stable placement key can spread replicas across explicit failure domains without maintaining a central per-object placement table.
15. **Heartbeats gate schedulability; they do not own reservations.** A fresh `Ready` heartbeat is required for live placement, while runtime-observed usage is reconciliation evidence only.
16. **The allocation ledger is authoritative for promised capacity.** Allocation IDs are idempotent, ledger mutations are generation-fenced, and durable implementations use atomic compare-and-swap to prevent scheduler replicas from overcommitting the same provider.

## Current scope

Implemented:

- normalized CPU/GPU capacity offers and workload requirements
- deterministic hard-constraint filtering and effective-cost scoring
- multi-provider fallback broker with provider-error isolation
- stale-snapshot rejection and reporting
- caller-owned provider circuit-breaker state
- provider-neutral interruption/drain/checkpoint state machine
- durable placement-claim and provider lease boundaries for race-free acquisition
- retry-idempotent claims and offer-scoped provider idempotency material
- sequential ranked lease acquisition with safe handling of indeterminate provider results
- async provider adapter boundary with no cloud SDK dependency
- AWS Spot/on-demand normalizer and interruption normalization
- GCP Spot/standard normalizer and preemption normalization
- Nebius preemptible/regular normalizer and preemption normalization
- observed fleet telemetry estimates for dynamic scheduler inputs
- concrete resource-provider topology with hierarchical parent links, integer inventories, required/forbidden traits, and generation-tagged snapshots
- hard allocation-candidate generation separated from soft economic/locality scoring
- deterministic replica ordering and fail-closed spreading across host, rack, zone, region, or provider failure domains
- topology-bound monotonic capacity heartbeats with `ready` / `draining` / `unavailable` states and freshness gating
- serializable allocation-ledger snapshots with idempotent commits/releases, expiry, generation fencing, and a provider-neutral durable CAS store boundary
- aggregate heartbeat-vs-ledger usage drift detection for reconciliation without letting observed runtime state silently rewrite reservations
- tests covering Spot economics, long-running interruption risk, egress, GPU/trust constraints, critical workload policy, cross-provider ranking, stale snapshots, provider health, interruption transitions, telemetry, and lease idempotency

## Adapter architecture

Provider-specific API clients should implement the source trait for their adapter (`AwsOfferSource`, `GcpOfferSource`, or `NebiusOfferSource`) and the capacity-leasing boundary used by the control plane. This keeps authentication, HTTP/cloud SDK selection, pagination, and provider API churn outside the scheduler core.

The capacity core must remain usable without any provider SDK dependency.

## Next slices

- derive static node topology/traits from runtime registration metadata while reporting dynamic inventory/usage through a separate capacity heartbeat; do not bloat membership/failure-detection gossip
- implement a production `AllocationLedgerStore` in the hosted control-plane datastore and exercise concurrent scheduler replicas against it
- add nested request groups for host + GPU/NVMe/NIC child-resource allocation
- add PACK/SPREAD placement groups for actor teams once concrete host topology is populated
- add reconciliation/rebalancing loops with bounded recovery QoS so repair cannot starve foreground workloads
- production AWS/GCP/Nebius API source and leasing clients in the Nulang Cloud control plane
- persistent rolling telemetry keyed by provider/region/offer
- durable claim-store implementation in the Cloud control-plane datastore
- reconciliation for indeterminate provider acquisitions
- CoreWeave and RunPod adapters
- Vast adapter for explicitly lower-trust opportunistic workloads
