//! OpenID AuthZEN Authorization API 1.0 adapter.
//!
//! The wire model follows the final specification's Subject-Action-Resource-
//! Context (SARC) information model for single access evaluations. This module
//! deliberately keeps protocol representation separate from Nulang's native
//! [`crate::authority::AuthorityGrant`] vocabulary: applications provide an
//! [`AuthorityGrantMapper`] that explicitly maps a SARC request to the exact
//! Nulang authority required for that operation.
//!
//! This avoids a dangerous implicit convention such as concatenating resource
//! and action strings into a capability token. Unknown/unmapped requests are
//! request errors, while a valid mapped request that lacks authority is a
//! normal authorization denial.

use crate::authority::AuthorityGrant;
use crate::authorization::{
    AuthorityDelegation, DecisionReason, Principal, PrincipalKind, UnixSeconds,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

/// Default HTTPS path for the AuthZEN 1.0 single Access Evaluation endpoint.
pub const ACCESS_EVALUATION_PATH: &str = "/access/v1/evaluation";

pub type AuthzenProperties = BTreeMap<String, Value>;

/// AuthZEN Subject: a human or machine principal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthzenSubject {
    #[serde(rename = "type")]
    pub r#type: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<AuthzenProperties>,
}

/// AuthZEN Action: the requested operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthzenAction {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<AuthzenProperties>,
}

/// AuthZEN Resource: the target of the access request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthzenResource {
    #[serde(rename = "type")]
    pub r#type: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<AuthzenProperties>,
}

/// AuthZEN Context: implementation-defined environmental attributes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthzenContext(pub BTreeMap<String, Value>);

/// AuthZEN 1.0 single Access Evaluation request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccessEvaluationRequest {
    pub subject: AuthzenSubject,
    pub action: AuthzenAction,
    pub resource: AuthzenResource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<AuthzenContext>,
}

/// AuthZEN Decision response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccessDecision {
    pub decision: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<AuthzenContext>,
}

impl From<&Principal> for AuthzenSubject {
    fn from(principal: &Principal) -> Self {
        let principal_type = match principal.kind() {
            PrincipalKind::Human => "human",
            PrincipalKind::Agent => "agent",
            PrincipalKind::Service => "service",
            PrincipalKind::Workload => "workload",
            PrincipalKind::Device => "device",
        };
        Self {
            r#type: principal_type.to_string(),
            id: principal.id().to_string(),
            properties: None,
        }
    }
}

impl TryFrom<&AuthzenSubject> for Principal {
    type Error = AuthzenError;

    fn try_from(subject: &AuthzenSubject) -> Result<Self, Self::Error> {
        let kind = match subject.r#type.as_str() {
            "human" => PrincipalKind::Human,
            "agent" => PrincipalKind::Agent,
            "service" => PrincipalKind::Service,
            "workload" => PrincipalKind::Workload,
            "device" => PrincipalKind::Device,
            other => return Err(AuthzenError::UnsupportedSubjectType(other.to_string())),
        };
        Principal::new(kind, subject.id.clone())
            .map_err(|_| AuthzenError::InvalidSubjectId(subject.id.clone()))
    }
}

/// Explicit policy bridge from AuthZEN's SARC model to one exact Nulang grant.
///
/// Resource/action semantics are application-specific. Keeping this as an
/// explicit trait prevents the protocol adapter from silently widening or
/// guessing authority.
pub trait AuthorityGrantMapper {
    fn required_grant(
        &self,
        request: &AccessEvaluationRequest,
    ) -> Result<AuthorityGrant, AuthzenError>;
}

/// Policy Decision Point adapter backed by one constrained Nulang delegation.
#[derive(Debug, Clone)]
pub struct DelegationPdp<M> {
    delegation: AuthorityDelegation,
    mapper: M,
}

impl<M> DelegationPdp<M>
where
    M: AuthorityGrantMapper,
{
    pub fn new(delegation: AuthorityDelegation, mapper: M) -> Self {
        Self { delegation, mapper }
    }

    pub fn delegation(&self) -> &AuthorityDelegation {
        &self.delegation
    }

    pub fn mapper(&self) -> &M {
        &self.mapper
    }

    /// Evaluate one valid AuthZEN request against the delegation.
    ///
    /// Structural/mapping failures return `Err` and belong to the transport
    /// error path. A policy denial remains a successful evaluation with
    /// `decision: false`, matching AuthZEN's separation between request errors
    /// and authorization outcomes.
    pub fn evaluate(
        &self,
        request: &AccessEvaluationRequest,
        now: UnixSeconds,
    ) -> Result<AccessDecision, AuthzenError> {
        let principal = Principal::try_from(&request.subject)?;
        let required = self.mapper.required_grant(request)?;
        let decision = self.delegation.authorize(&principal, &required, now);

        if decision.is_allowed() {
            return Ok(AccessDecision {
                decision: true,
                context: None,
            });
        }

        let reason = decision
            .reason()
            .map(reason_code)
            .unwrap_or("denied")
            .to_string();
        let mut context = BTreeMap::new();
        context.insert("reason".to_string(), Value::String(reason));

        Ok(AccessDecision {
            decision: false,
            context: Some(AuthzenContext(context)),
        })
    }
}

fn reason_code(reason: &DecisionReason) -> &'static str {
    match reason {
        DecisionReason::PrincipalMismatch { .. } => "principal_mismatch",
        DecisionReason::NotYetValid { .. } => "not_yet_valid",
        DecisionReason::Expired { .. } => "expired",
        DecisionReason::MissingGrant(_) => "missing_grant",
    }
}

/// Invalid AuthZEN-to-Nulang evaluation input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthzenError {
    UnsupportedSubjectType(String),
    InvalidSubjectId(String),
    UnmappedRequest,
}

impl fmt::Display for AuthzenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSubjectType(kind) => {
                write!(f, "unsupported AuthZEN subject type: {kind}")
            }
            Self::InvalidSubjectId(id) => write!(f, "invalid AuthZEN subject id: {id:?}"),
            Self::UnmappedRequest => write!(f, "AuthZEN request has no authority mapping"),
        }
    }
}

impl Error for AuthzenError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_decision_serializes_without_context() {
        let value = serde_json::to_value(AccessDecision {
            decision: true,
            context: None,
        })
        .unwrap();
        assert_eq!(value, serde_json::json!({"decision": true}));
    }

    #[test]
    fn endpoint_path_matches_authzen_v1_default() {
        assert_eq!(ACCESS_EVALUATION_PATH, "/access/v1/evaluation");
    }
}
