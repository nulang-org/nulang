//! Pure application authorization policy kernel.
//!
//! This crate is intentionally independent of the Nulang compiler/runtime.
//! It consumes shared principals from `nulang-security` and evaluates trusted
//! application facts with deterministic deny-by-default semantics.

use nulang_security::Principal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

macro_rules! string_id_type {
    ($name:ident, $kind:literal) => {
        #[derive(
            Debug,
            Clone,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, PolicyError> {
                let value = value.into();
                if value.trim().is_empty() {
                    return Err(PolicyError::EmptyIdentifier($kind));
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id_type!(PolicyDomainId, "policy domain");
string_id_type!(ActionId, "action");
string_id_type!(ResourceType, "resource type");
string_id_type!(RoleId, "role");
string_id_type!(RelationId, "relation");
string_id_type!(RuleId, "rule");

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ResourceRef {
    kind: ResourceType,
    id: String,
}

impl ResourceRef {
    pub fn new(kind: ResourceType, id: impl Into<String>) -> Result<Self, PolicyError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(PolicyError::EmptyIdentifier("resource id"));
        }
        Ok(Self { kind, id })
    }

    pub fn kind(&self) -> &ResourceType {
        &self.kind
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

/// Explicit resource scope. `id == None` means every resource of the named
/// type; there are no string wildcards or glob semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceScope {
    kind: ResourceType,
    id: Option<String>,
}

impl ResourceScope {
    pub fn all(kind: ResourceType) -> Self {
        Self { kind, id: None }
    }

    pub fn exact(resource: ResourceRef) -> Self {
        Self {
            kind: resource.kind,
            id: Some(resource.id),
        }
    }

    fn matches(&self, resource: &ResourceRef) -> bool {
        self.kind == resource.kind
            && self
                .id
                .as_deref()
                .is_none_or(|expected| expected == resource.id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyEffect {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributeSource {
    Subject,
    Resource,
    Context,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributeCondition {
    source: AttributeSource,
    key: String,
    equals: String,
}

impl AttributeCondition {
    pub fn new(
        source: AttributeSource,
        key: impl Into<String>,
        equals: impl Into<String>,
    ) -> Result<Self, PolicyError> {
        let key = key.into();
        if key.trim().is_empty() {
            return Err(PolicyError::EmptyIdentifier("attribute key"));
        }
        Ok(Self {
            source,
            key,
            equals: equals.into(),
        })
    }

    fn matches(&self, facts: &PolicyFacts) -> bool {
        let attributes = match self.source {
            AttributeSource::Subject => &facts.subject_attributes,
            AttributeSource::Resource => &facts.resource_attributes,
            AttributeSource::Context => &facts.context,
        };
        attributes.get(&self.key) == Some(&self.equals)
    }
}

/// First-slice policy predicate vocabulary.
///
/// Relationship predicates are direct only. Graph traversal and computed
/// relations belong in a later relationship resolver, not in this kernel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Predicate {
    Role(RoleId),
    Attribute(AttributeCondition),
    Relation(RelationId),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Relationship {
    subject: Principal,
    relation: RelationId,
    resource: ResourceRef,
}

impl Relationship {
    pub fn new(subject: Principal, relation: RelationId, resource: ResourceRef) -> Self {
        Self {
            subject,
            relation,
            resource,
        }
    }
}

/// Trusted authorization facts supplied by the embedding application.
///
/// This kernel does not authenticate role membership, attributes, or relation
/// tuples. Callers must derive them from trusted state before evaluation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyFacts {
    roles: BTreeSet<RoleId>,
    subject_attributes: BTreeMap<String, String>,
    resource_attributes: BTreeMap<String, String>,
    context: BTreeMap<String, String>,
    relationships: BTreeSet<Relationship>,
}

impl PolicyFacts {
    pub fn new(
        subject_attributes: BTreeMap<String, String>,
        resource_attributes: BTreeMap<String, String>,
        context: BTreeMap<String, String>,
        roles: impl IntoIterator<Item = RoleId>,
        relationships: impl IntoIterator<Item = Relationship>,
    ) -> Self {
        Self {
            roles: roles.into_iter().collect(),
            subject_attributes,
            resource_attributes,
            context,
            relationships: relationships.into_iter().collect(),
        }
    }

    pub fn add_role(&mut self, role: RoleId) {
        self.roles.insert(role);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRequest {
    domain: PolicyDomainId,
    subject: Principal,
    action: ActionId,
    resource: ResourceRef,
    facts: PolicyFacts,
}

impl PolicyRequest {
    pub fn new(
        domain: PolicyDomainId,
        subject: Principal,
        action: ActionId,
        resource: ResourceRef,
        facts: PolicyFacts,
    ) -> Self {
        Self {
            domain,
            subject,
            action,
            resource,
            facts,
        }
    }

    pub fn domain(&self) -> &PolicyDomainId {
        &self.domain
    }

    pub fn subject(&self) -> &Principal {
        &self.subject
    }

    pub fn action(&self) -> &ActionId {
        &self.action
    }

    pub fn resource(&self) -> &ResourceRef {
        &self.resource
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRule {
    id: RuleId,
    effect: PolicyEffect,
    action: ActionId,
    scope: ResourceScope,
    predicate: Predicate,
}

impl PolicyRule {
    pub fn new(
        id: RuleId,
        effect: PolicyEffect,
        action: ActionId,
        scope: ResourceScope,
        predicate: Predicate,
    ) -> Self {
        Self {
            id,
            effect,
            action,
            scope,
            predicate,
        }
    }

    fn matches(&self, request: &PolicyRequest) -> bool {
        if self.action != request.action || !self.scope.matches(&request.resource) {
            return false;
        }

        match &self.predicate {
            Predicate::Role(role) => request.facts.roles.contains(role),
            Predicate::Attribute(condition) => condition.matches(&request.facts),
            Predicate::Relation(relation) => request.facts.relationships.iter().any(|tuple| {
                tuple.subject == request.subject
                    && tuple.relation == *relation
                    && tuple.resource == request.resource
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicySet {
    domain: PolicyDomainId,
    rules: BTreeMap<RuleId, PolicyRule>,
}

impl PolicySet {
    pub fn new(
        domain: PolicyDomainId,
        rules: impl IntoIterator<Item = PolicyRule>,
    ) -> Result<Self, PolicyError> {
        let mut indexed = BTreeMap::new();
        for rule in rules {
            if indexed.contains_key(&rule.id) {
                return Err(PolicyError::DuplicateRuleId(rule.id));
            }
            indexed.insert(rule.id.clone(), rule);
        }
        Ok(Self {
            domain,
            rules: indexed,
        })
    }

    /// Evaluate with deterministic deny-overrides-allow semantics.
    ///
    /// Rules are indexed by `RuleId`, so if multiple denies or allows match,
    /// the lexicographically smallest matching ID is the stable explanation.
    pub fn evaluate(&self, request: &PolicyRequest) -> PolicyDecision {
        if self.domain != request.domain {
            return PolicyDecision::deny(PolicyDecisionReason::DomainMismatch {
                policy_domain: self.domain.clone(),
                request_domain: request.domain.clone(),
            });
        }

        let mut first_allow = None;
        for (id, rule) in &self.rules {
            if !rule.matches(request) {
                continue;
            }
            match rule.effect {
                PolicyEffect::Deny => {
                    return PolicyDecision::deny(PolicyDecisionReason::ExplicitDeny(id.clone()))
                }
                PolicyEffect::Allow if first_allow.is_none() => first_allow = Some(id.clone()),
                PolicyEffect::Allow => {}
            }
        }

        match first_allow {
            Some(id) => PolicyDecision::allow(PolicyDecisionReason::ExplicitAllow(id)),
            None => PolicyDecision::deny(PolicyDecisionReason::NoMatchingAllow),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecisionReason {
    DomainMismatch {
        policy_domain: PolicyDomainId,
        request_domain: PolicyDomainId,
    },
    ExplicitDeny(RuleId),
    ExplicitAllow(RuleId),
    NoMatchingAllow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDecision {
    allowed: bool,
    reason: PolicyDecisionReason,
}

impl PolicyDecision {
    fn allow(reason: PolicyDecisionReason) -> Self {
        Self {
            allowed: true,
            reason,
        }
    }

    fn deny(reason: PolicyDecisionReason) -> Self {
        Self {
            allowed: false,
            reason,
        }
    }

    pub const fn is_allowed(&self) -> bool {
        self.allowed
    }

    pub fn reason(&self) -> &PolicyDecisionReason {
        &self.reason
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyError {
    EmptyIdentifier(&'static str),
    DuplicateRuleId(RuleId),
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyIdentifier(kind) => write!(f, "{kind} identifier must not be empty"),
            Self::DuplicateRuleId(id) => write!(f, "duplicate policy rule id: {id}"),
        }
    }
}

impl Error for PolicyError {}
