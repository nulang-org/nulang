//! Shared native compilation optimization policy.
//!
//! The JIT and whole-module native backend should use the same explicit
//! latency/quality profiles so development compilation can optimize for
//! turnaround time while hot/release compilation spends more work on code
//! quality.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeOptimizationProfile {
    Fast,
    Optimized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CraneliftOptimizationSettings {
    pub opt_level: &'static str,
    pub regalloc_algorithm: &'static str,
}

impl NativeOptimizationProfile {
    pub const fn cranelift_settings(self) -> CraneliftOptimizationSettings {
        match self {
            Self::Fast => CraneliftOptimizationSettings {
                opt_level: "none",
                regalloc_algorithm: "single_pass",
            },
            Self::Optimized => CraneliftOptimizationSettings {
                opt_level: "speed",
                regalloc_algorithm: "backtracking",
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_profile_prioritizes_compile_latency() {
        assert_eq!(
            NativeOptimizationProfile::Fast.cranelift_settings(),
            CraneliftOptimizationSettings {
                opt_level: "none",
                regalloc_algorithm: "single_pass",
            }
        );
    }

    #[test]
    fn optimized_profile_prioritizes_runtime_quality() {
        assert_eq!(
            NativeOptimizationProfile::Optimized.cranelift_settings(),
            CraneliftOptimizationSettings {
                opt_level: "speed",
                regalloc_algorithm: "backtracking",
            }
        );
    }
}
