# nulang-capacity

Provider-neutral capacity scoring and placement primitives for Nulang Cloud.

This crate is deliberately separated from the Nulang language/runtime and from cloud SDKs. Provider integrations translate their native inventory, pricing, interruption, and availability signals into `CapacityOffer`; the scheduler then applies hard constraints and ranks the remaining offers by expected cost to complete the job.

## Design rules

1. **Providers normalize; they do not schedule.** AWS, GCP, Nebius, CoreWeave, RunPod, Vast, and future providers should all emit the same `CapacityOffer` model.
2. **Hard constraints run before scoring.** Architecture, CPU, memory, region, trust, GPU model/count, and VRAM are eligibility requirements, not soft preferences.
3. **Optimize completed-job economics.** Ranking includes observed throughput, interruption recovery, startup latency, capacity confidence, storage, and egress rather than comparing hourly price alone.
4. **Critical workloads default away from interruptible capacity.** Checkpointable and ephemeral workloads can exploit Spot/preemptible markets much more aggressively.
5. **Telemetry should replace static assumptions.** `throughput_score`, interruption rate, startup latency, and capacity confidence are intended to be fed by observed Nulang Cloud fleet data.

## Current scope

The first slice implements:

- normalized CPU/GPU capacity offers
- workload requirements and trust tiers
- deterministic hard-constraint filtering
- effective-cost scoring
- deterministic ranking
- tests covering short Spot jobs, interruption-heavy long jobs, egress costs, GPU/trust constraints, and critical-workload fallback behavior

## Next slices

- provider-neutral interruption/drain/checkpoint state machine
- provider adapter boundary
- AWS Spot adapter
- GCP Spot adapter
- Nebius preemptible/GPU adapter
- observed performance and acquisition telemetry
- fallback ladder from preferred Spot capacity to alternate pools/providers and finally on-demand

The scheduler core must remain usable without any provider SDK dependency.
