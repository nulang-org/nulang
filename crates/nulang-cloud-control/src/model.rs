use nulang_capacity::{Architecture, TrustTier};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Ready,
    Draining,
    Suspect,
    Unreachable,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceleratorCapacity {
    pub count_available: u16,
    pub vram_mib_each: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeResources {
    pub cpu_millis_total: u32,
    pub cpu_millis_available: u32,
    pub memory_mib_total: u64,
    pub memory_mib_available: u64,
    #[serde(default)]
    pub accelerators: BTreeMap<String, AcceleratorCapacity>,
}

impl NodeResources {
    pub fn reserve(&mut self, request: &ResourceRequest) -> bool {
        if self.cpu_millis_available < request.cpu_millis
            || self.memory_mib_available < request.memory_mib
        {
            return false;
        }

        if let Some(accelerator) = &request.accelerator {
            let Some((model, _)) = matching_accelerator(&self.accelerators, accelerator) else {
                return false;
            };
            let pool = self
                .accelerators
                .get_mut(&model)
                .expect("matching accelerator must still exist");
            pool.count_available -= accelerator.count;
        }

        self.cpu_millis_available -= request.cpu_millis;
        self.memory_mib_available -= request.memory_mib;
        true
    }

    pub fn fits(&self, request: &ResourceRequest) -> bool {
        if self.cpu_millis_available < request.cpu_millis
            || self.memory_mib_available < request.memory_mib
        {
            return false;
        }

        request
            .accelerator
            .as_ref()
            .map(|required| matching_accelerator(&self.accelerators, required).is_some())
            .unwrap_or(true)
    }
}

fn matching_accelerator(
    accelerators: &BTreeMap<String, AcceleratorCapacity>,
    request: &AcceleratorRequirement,
) -> Option<(String, AcceleratorCapacity)> {
    accelerators.iter().find_map(|(model, capacity)| {
        let model_matches = request
            .model
            .as_ref()
            .map(|required| model.eq_ignore_ascii_case(required))
            .unwrap_or(true);
        if model_matches
            && capacity.count_available >= request.count
            && capacity.vram_mib_each >= request.min_vram_mib_each
        {
            Some((model.clone(), capacity.clone()))
        } else {
            None
        }
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeDescriptor {
    pub node_id: u64,
    pub state: NodeState,
    pub region: String,
    pub zone: Option<String>,
    pub architecture: Architecture,
    pub trust_tier: TrustTier,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
    pub resources: NodeResources,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceleratorRequirement {
    pub model: Option<String>,
    pub count: u16,
    pub min_vram_mib_each: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRequest {
    pub cpu_millis: u32,
    pub memory_mib: u64,
    pub accelerator: Option<AcceleratorRequirement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementConstraints {
    pub architecture: Option<Architecture>,
    #[serde(default)]
    pub allowed_regions: Vec<String>,
    #[serde(default)]
    pub allowed_zones: Vec<String>,
    pub min_trust_tier: TrustTier,
    #[serde(default)]
    pub required_labels: BTreeMap<String, String>,
    #[serde(default)]
    pub required_capabilities: BTreeSet<String>,
    pub max_replicas_per_node: Option<u32>,
}

impl Default for PlacementConstraints {
    fn default() -> Self {
        Self {
            architecture: None,
            allowed_regions: Vec::new(),
            allowed_zones: Vec::new(),
            min_trust_tier: TrustTier::Untrusted,
            required_labels: BTreeMap::new(),
            required_capabilities: BTreeSet::new(),
            max_replicas_per_node: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlacementPreferences {
    /// Ordered strongest-to-weakest. A region not listed gets no locality bonus.
    #[serde(default)]
    pub preferred_regions: Vec<String>,
    /// Penalize zones that already host retained/planned replicas.
    pub spread_by_zone: bool,
    /// Prefer nodes with more proportional CPU + memory headroom after placement.
    pub prefer_headroom: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentSpec {
    pub deployment_id: String,
    pub revision: u64,
    pub replicas: u32,
    pub resources: ResourceRequest,
    #[serde(default)]
    pub constraints: PlacementConstraints,
    #[serde(default)]
    pub preferences: PlacementPreferences,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationCause {
    DeploymentChanged,
    NodeChanged,
    AllocationFailed,
    CapacityChanged,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evaluation {
    pub evaluation_id: String,
    pub deployment_id: String,
    pub revision: u64,
    pub cause: EvaluationCause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllocationState {
    Starting,
    Running,
    Failed,
    Stopped,
}

impl AllocationState {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedAllocation {
    pub deployment_id: String,
    pub revision: u64,
    pub replica: u32,
    pub node_id: u64,
    /// Monotonic fencing epoch for this logical deployment replica.
    pub epoch: u64,
    pub state: AllocationState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionCode {
    NodeNotReady,
    ArchitectureMismatch,
    RegionNotAllowed,
    ZoneNotAllowed,
    TrustTierTooLow,
    MissingLabel,
    MissingCapability,
    InsufficientCpu,
    InsufficientMemory,
    InsufficientAccelerator,
    MaxReplicasPerNode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejection {
    pub code: RejectionCode,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRejection {
    pub node_id: u64,
    pub reasons: Vec<Rejection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ScoreBreakdown {
    pub region_locality: i64,
    pub zone_spread: i64,
    pub node_spread: i64,
    pub headroom: i64,
}

impl ScoreBreakdown {
    pub fn total(self) -> i64 {
        self.region_locality + self.zone_spread + self.node_spread + self.headroom
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedAllocation {
    pub replica: u32,
    pub node_id: u64,
    pub epoch: u64,
    pub score: ScoreBreakdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupersededReason {
    Duplicate,
    StaleRevision,
    PlacementInvalid,
    ScaleDown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupersededAllocation {
    pub allocation: ObservedAllocation,
    pub reason: SupersededReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedPlacement {
    pub unscheduled_replicas: Vec<u32>,
    pub considered_nodes: usize,
    pub rejection_counts: BTreeMap<RejectionCode, usize>,
    pub node_rejections: Vec<NodeRejection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementPlan {
    pub evaluation_id: String,
    pub deployment_id: String,
    pub revision: u64,
    pub retained: Vec<ObservedAllocation>,
    pub placements: Vec<PlannedAllocation>,
    pub superseded: Vec<SupersededAllocation>,
    pub blocked: Option<BlockedPlacement>,
}

impl PlacementPlan {
    pub fn is_complete(&self) -> bool {
        self.blocked.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    DeploymentMismatch {
        evaluation: String,
        deployment: String,
    },
    RevisionMismatch {
        evaluation: u64,
        deployment: u64,
    },
}
