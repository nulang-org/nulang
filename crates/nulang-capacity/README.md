# nulang-capacity

Provider-neutral capacity scoring and placement primitives for Nulang Cloud.

This crate is deliberately separated from the Nulang language/runtime and from cloud SDKs. Provider integrations translate native inventory, pricing, interruption, and availability signals into `CapacityOffer`; the broker applies hard constraints and ranks the remaining offers by expected cost to complete the job.

## Design rules

1. **Providers normalize; they do not schedule.** AWS, GCP, Nebius, CoreWeave, RunPod, Vast, and future providers emit the same `CapacityOffer` model.
2. **Hard constraints run before scoring.** Architecture, CPU, memory, region, trust, interruptibility, GPU model/count, and VRAM are eligibility requirements rather than soft preferences.
3. **Optimize completed-job economics.** Ranking includes observed throughput, interruption recovery, startup latency, acquisition confidence, storage, and egress rather than comparing hourly price alone.
4. **Interruptibility is explicit policy.** Critical workloads can reject Spot/preemptible capacity entirely; checkpointable and ephemeral workloads can opt in.
5. **Provider failures are isolated.** The broker continues ranking healthy providers when one provider endpoint fails.
6. **Telemetry replaces static assumptions.** Observed fleet data can update throughput, interruption probability, startup percentiles, and acquisition confidence before scoring.

## Current scope

Implemented:

- normalized CPU/GPU capacity offers and workload requirements
- deterministic hard-constraint filtering and effective-cost scoring
- multi-provider fallback broker
- provider-neutral interruption/drain/checkpoint state machine
- async provider adapter boundary with no cloud SDK dependency
- AWS Spot/on-demand normalizer and interruption normalization
- GCP Spot/standard normalizer and preemption normalization
- Nebius preemptible/regular normalizer and preemption normalization
- observed fleet telemetry estimates for dynamic scheduler inputs
- tests covering Spot economics, long-running interruption risk, egress, GPU/trust constraints, critical workload policy, cross-provider ranking, interruption transitions, and telemetry

## Adapter architecture

Provider-specific API clients should implement the source trait for their adapter (`AwsOfferSource`, `GcpOfferSource`, or `NebiusOfferSource`). This keeps authentication, HTTP/cloud SDK selection, pagination, and provider API churn outside the scheduler core.

The capacity core must remain usable without any provider SDK dependency.

## Next slices

- production AWS/GCP/Nebius API source clients in the Nulang Cloud control plane
- persistent rolling telemetry keyed by provider/region/offer
- stale-snapshot rejection and circuit breaking
- lease acquisition/retry orchestration
- CoreWeave and RunPod adapters
- Vast adapter for explicitly lower-trust opportunistic workloads
