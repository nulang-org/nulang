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
10. **Ambiguous acquisition never falls through.** If a provider may have allocated capacity but its response was lost—or returns a success whose identity does not match the exact request—the placement remains claimed and must be reconciled before another provider is tried.
11. **Reconciliation is fail-closed.** `Pending`, provider lookup errors, and invalid recovered lease identities keep the durable claim and prohibit fallback. Only an authoritative `ConfirmedAbsent` result may resume the ranked ladder, starting strictly after the reconciled offer.
12. **Telemetry replaces static assumptions.** Observed fleet data can update throughput, interruption probability, startup percentiles, and acquisition confidence before scoring.
13. **Runtime eligibility remains a hard gate after economic ranking.** A cheap provider offer is not schedulable unless the bound worker pool supplies every required runtime feature and the minimum isolation level. Extra runtime features never imply deployment authority; authority remains the admission policy's responsibility.

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
- successful provider lease response validation against provider, offer, job, placement token, state, and non-empty lease identity
- sequential ranked lease acquisition with safe handling of indeterminate and invalid-success provider results
- exact `LeaseRequest` retention for ambiguous acquisition so reconciliation preserves logical placement identity
- provider-neutral lease reconciliation boundary with `Acquired`, `ConfirmedAbsent`, and `Pending` outcomes
- safe fallback resumption only after confirmed absence, using the same placement token and only candidates after the reconciled offer
- recovered-lease identity validation before execution
- async provider adapter boundary with no cloud SDK dependency
- AWS Spot/on-demand normalizer and interruption normalization
- GCP Spot/standard normalizer and preemption normalization
- Nebius preemptible/regular normalizer and preemption normalization
- observed fleet telemetry estimates for dynamic scheduler inputs
- runtime-aware execution targets with deterministic feature/isolation eligibility filtering that preserves broker ranking
- tests covering Spot economics, long-running interruption risk, egress, GPU/trust constraints, critical workload policy, cross-provider ranking, stale snapshots, provider health, interruption transitions, telemetry, lease idempotency, lease-response integrity, reconciliation, and runtime/isolation eligibility

## Adapter architecture

Provider-specific API clients should implement the source trait for their adapter (`AwsOfferSource`, `GcpOfferSource`, or `NebiusOfferSource`) and the capacity-leasing/reconciliation boundaries used by the control plane. This keeps authentication, HTTP/cloud SDK selection, pagination, and provider API churn outside the scheduler core.

The capacity core must remain usable without any provider SDK dependency.

A reconciliation adapter must use the strongest provider-native request/idempotency identity available. `ConfirmedAbsent` is safety-critical and must be returned only when the provider can authoritatively establish that the original request did not allocate capacity. Lack of evidence is `Pending`, not absence.

The hosted control plane should translate validated deployment admission into `ExecutionRequirements`, bind configured worker-pool runtime facts as `RuntimeEnvelope`s, and filter economically ranked candidates before final authoritative admission and scheduling. `nulang-capacity` intentionally does not import the compiler/runtime crate to perform that translation itself.

## Next slices

- production AWS/GCP/Nebius API source, leasing, and reconciliation clients in the Nulang Cloud control plane
- persistent rolling telemetry keyed by provider/region/offer
- durable claim-store implementation in the Cloud control-plane datastore
- persisted reconciliation state / retry scheduling for ambiguous acquisitions
- CoreWeave and RunPod adapters
- Vast adapter for explicitly lower-trust opportunistic workloads
