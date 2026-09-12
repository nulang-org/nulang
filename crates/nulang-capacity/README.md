# nulang-capacity

Provider-neutral capacity scoring and placement primitives for Nulang Cloud.

This crate is deliberately separated from the Nulang language/runtime and from cloud SDKs. Provider integrations translate their native inventory, pricing, interruption, and availability signals into `CapacityOffer`; the scheduler then applies hard constraints and ranks the remaining offers by expected cost to complete the job.

## Design rules

1. **Providers normalize; they do not schedule.** AWS, GCP, Nebius, CoreWeave, RunPod, Vast, and future providers should all emit the same `CapacityOffer` model.
2. **Hard constraints run before scoring.** Architecture, CPU, memory, region, trust, GPU model/count, VRAM, and interruptibility policy are eligibility requirements, not soft preferences.
3. **Optimize completed-job economics.** Ranking includes observed throughput, interruption recovery, startup latency, capacity confidence, storage, and egress rather than comparing hourly price alone.
4. **Interruptibility is explicit policy.** Critical workloads can reject Spot/preemptible capacity outright; checkpointable and ephemeral workloads can opt in aggressively.
5. **Telemetry should replace static assumptions.** `throughput_score`, interruption rate, startup latency, and capacity confidence are intended to be fed by observed Nulang Cloud fleet data.

## Current scope

The initial implementation includes:

- normalized CPU/GPU capacity offers
- workload requirements and trust tiers
- explicit interruptible-capacity policy
- deterministic hard-constraint filtering
- effective-cost scoring and deterministic ranking
- provider-neutral interruption notices
- drain/checkpoint/release/reschedule worker state machine
- async `CapacityProvider` adapter boundary with normalized snapshots/errors
- tests covering short Spot jobs, interruption-heavy long jobs, egress costs, GPU/trust constraints, critical-workload policy, and interruption-state behavior

## Next slices

- AWS Spot adapter
- GCP Spot adapter
- Nebius preemptible/GPU adapter
- observed performance and acquisition telemetry
- fallback ladder from preferred interruptible capacity to alternate pools/providers and finally on-demand
- CoreWeave and RunPod adapters
- Vast adapter for explicitly lower-trust opportunistic workloads

The scheduler core must remain usable without any provider SDK dependency.
