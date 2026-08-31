//! Deployment adapters for Nulang Web apps.
//!
//! Adapters turn the deployment IR (`dist/nulang-app.ir.json`) into a
//! target-specific artifact set. The default adapter is Nulang Cloud; other
//! adapters produce static-host or Docker outputs without changing the IR.

pub mod docker;
pub mod nulang_cloud;
pub mod static_host;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    NulangCloud,
    StaticHost,
    Docker,
}

impl AdapterKind {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "nulang_cloud" | "cloud" => Some(Self::NulangCloud),
            "static_host" | "static" => Some(Self::StaticHost),
            "docker" => Some(Self::Docker),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AdapterKind::NulangCloud => "nulang_cloud",
            AdapterKind::StaticHost => "static_host",
            AdapterKind::Docker => "docker",
        }
    }
}
