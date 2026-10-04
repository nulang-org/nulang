use nulang_policy::{
    ActionId, AttributeCondition, AttributeSource, PolicyDecisionReason, PolicyDomainId, PolicyEffect,
    PolicyFacts, PolicyRequest, PolicyRule, PolicySet, Predicate, RelationId, Relationship,
    ResourceRef, ResourceScope, ResourceType, RoleId, RuleId,
};
use nulang_security::{Principal, PrincipalKind};
use std::collections::BTreeMap;

fn principal(id: &str) -> Principal {
    Principal::new(PrincipalKind::Human, id).unwrap()
}

fn domain(id: &str) -> PolicyDomainId {
    PolicyDomainId::new(id).unwrap()
}

fn action(id: &str) -> ActionId {
    ActionId::new(id).unwrap()
}

fn resource(kind: &str, id: &str) -> ResourceRef {
    ResourceRef::new(ResourceType::new(kind).unwrap(), id).unwrap()
}

fn request(
    domain_id: &str,
    subject: Principal,
    action_id: &str,
    resource: ResourceRef,
    facts: PolicyFacts,
) -> PolicyRequest {
    PolicyRequest::new(domain(domain_id), subject, action(action_id), resource, facts)
}

#[test]
fn rbac_role_allows_matching_action_and_resource_type() {
    let admin = RoleId::new("admin").unwrap();
    let rule = PolicyRule::new(
        RuleId::new("allow-admin-read").unwrap(),
        PolicyEffect::Allow,
        action("invoice.read"),
        ResourceScope::all(ResourceType::new("invoice").unwrap()),
        Predicate::Role(admin.clone()),
    );
    let set = PolicySet::new(domain("tenant:acme/app-policy"), [rule]).unwrap();
    let mut facts = PolicyFacts::default();
    facts.add_role(admin);

    let decision = set.evaluate(&request(
        "tenant:acme/app-policy",
        principal("human:alice"),
        "invoice.read",
        resource("invoice", "inv_123"),
        facts,
    ));

    assert!(decision.is_allowed());
    assert_eq!(
        decision.reason(),
        &PolicyDecisionReason::ExplicitAllow(RuleId::new("allow-admin-read").unwrap())
    );
}

#[test]
fn no_matching_allow_is_denied_by_default() {
    let set = PolicySet::new(domain("tenant:acme/app-policy"), []).unwrap();

    let decision = set.evaluate(&request(
        "tenant:acme/app-policy",
        principal("human:alice"),
        "invoice.read",
        resource("invoice", "inv_123"),
        PolicyFacts::default(),
    ));

    assert!(!decision.is_allowed());
    assert_eq!(decision.reason(), &PolicyDecisionReason::NoMatchingAllow);
}

#[test]
fn explicit_deny_overrides_a_matching_allow() {
    let admin = RoleId::new("admin").unwrap();
    let suspended = RoleId::new("suspended").unwrap();
    let allow = PolicyRule::new(
        RuleId::new("allow-admin-read").unwrap(),
        PolicyEffect::Allow,
        action("invoice.read"),
        ResourceScope::all(ResourceType::new("invoice").unwrap()),
        Predicate::Role(admin.clone()),
    );
    let deny = PolicyRule::new(
        RuleId::new("deny-suspended-read").unwrap(),
        PolicyEffect::Deny,
        action("invoice.read"),
        ResourceScope::all(ResourceType::new("invoice").unwrap()),
        Predicate::Role(suspended.clone()),
    );
    let set = PolicySet::new(domain("tenant:acme/app-policy"), [allow, deny]).unwrap();
    let mut facts = PolicyFacts::default();
    facts.add_role(admin);
    facts.add_role(suspended);

    let decision = set.evaluate(&request(
        "tenant:acme/app-policy",
        principal("human:alice"),
        "invoice.read",
        resource("invoice", "inv_123"),
        facts,
    ));

    assert_eq!(
        decision.reason(),
        &PolicyDecisionReason::ExplicitDeny(RuleId::new("deny-suspended-read").unwrap())
    );
}

#[test]
fn abac_matches_exact_trusted_attributes() {
    let rule = PolicyRule::new(
        RuleId::new("allow-finance-dept").unwrap(),
        PolicyEffect::Allow,
        action("invoice.approve"),
        ResourceScope::all(ResourceType::new("invoice").unwrap()),
        Predicate::Attribute(AttributeCondition::new(
            AttributeSource::Subject,
            "department",
            "finance",
        )
        .unwrap()),
    );
    let set = PolicySet::new(domain("tenant:acme/app-policy"), [rule]).unwrap();
    let mut subject_attributes = BTreeMap::new();
    subject_attributes.insert("department".to_string(), "finance".to_string());
    let facts = PolicyFacts::new(subject_attributes, BTreeMap::new(), BTreeMap::new(), [], []);

    assert!(set
        .evaluate(&request(
            "tenant:acme/app-policy",
            principal("human:alice"),
            "invoice.approve",
            resource("invoice", "inv_123"),
            facts,
        ))
        .is_allowed());
}

#[test]
fn rebac_direct_relation_allows_only_the_related_subject_and_resource() {
    let relation = RelationId::new("owner").unwrap();
    let rule = PolicyRule::new(
        RuleId::new("allow-owner-read").unwrap(),
        PolicyEffect::Allow,
        action("document.read"),
        ResourceScope::all(ResourceType::new("document").unwrap()),
        Predicate::Relation(relation.clone()),
    );
    let set = PolicySet::new(domain("tenant:acme/app-policy"), [rule]).unwrap();
    let alice = principal("human:alice");
    let doc = resource("document", "doc_1");
    let relationship = Relationship::new(alice.clone(), relation, doc.clone());
    let facts = PolicyFacts::new(BTreeMap::new(), BTreeMap::new(), BTreeMap::new(), [], [relationship]);

    assert!(set
        .evaluate(&request(
            "tenant:acme/app-policy",
            alice,
            "document.read",
            doc,
            facts,
        ))
        .is_allowed());
}

#[test]
fn policy_domain_mismatch_fails_closed_before_rules() {
    let role = RoleId::new("admin").unwrap();
    let rule = PolicyRule::new(
        RuleId::new("allow-admin-read").unwrap(),
        PolicyEffect::Allow,
        action("invoice.read"),
        ResourceScope::all(ResourceType::new("invoice").unwrap()),
        Predicate::Role(role.clone()),
    );
    let set = PolicySet::new(domain("tenant:acme/app-policy"), [rule]).unwrap();
    let mut facts = PolicyFacts::default();
    facts.add_role(role);

    let decision = set.evaluate(&request(
        "tenant:other/app-policy",
        principal("human:alice"),
        "invoice.read",
        resource("invoice", "inv_123"),
        facts,
    ));

    assert!(matches!(
        decision.reason(),
        PolicyDecisionReason::DomainMismatch { .. }
    ));
}

#[test]
fn exact_resource_scope_does_not_leak_to_other_resources() {
    let admin = RoleId::new("admin").unwrap();
    let rule = PolicyRule::new(
        RuleId::new("allow-specific-invoice").unwrap(),
        PolicyEffect::Allow,
        action("invoice.read"),
        ResourceScope::exact(resource("invoice", "inv_123")),
        Predicate::Role(admin.clone()),
    );
    let set = PolicySet::new(domain("tenant:acme/app-policy"), [rule]).unwrap();
    let mut facts = PolicyFacts::default();
    facts.add_role(admin);

    let decision = set.evaluate(&request(
        "tenant:acme/app-policy",
        principal("human:alice"),
        "invoice.read",
        resource("invoice", "inv_999"),
        facts,
    ));

    assert_eq!(decision.reason(), &PolicyDecisionReason::NoMatchingAllow);
}

#[test]
fn duplicate_rule_ids_are_rejected() {
    let id = RuleId::new("duplicate").unwrap();
    let make_rule = || {
        PolicyRule::new(
            id.clone(),
            PolicyEffect::Allow,
            action("invoice.read"),
            ResourceScope::all(ResourceType::new("invoice").unwrap()),
            Predicate::Role(RoleId::new("admin").unwrap()),
        )
    };

    assert!(PolicySet::new(
        domain("tenant:acme/app-policy"),
        [make_rule(), make_rule()]
    )
    .is_err());
}

#[test]
fn matching_rule_selection_is_deterministic_by_rule_id() {
    let role = RoleId::new("admin").unwrap();
    let later = PolicyRule::new(
        RuleId::new("z-allow").unwrap(),
        PolicyEffect::Allow,
        action("invoice.read"),
        ResourceScope::all(ResourceType::new("invoice").unwrap()),
        Predicate::Role(role.clone()),
    );
    let earlier = PolicyRule::new(
        RuleId::new("a-allow").unwrap(),
        PolicyEffect::Allow,
        action("invoice.read"),
        ResourceScope::all(ResourceType::new("invoice").unwrap()),
        Predicate::Role(role.clone()),
    );
    let set = PolicySet::new(domain("tenant:acme/app-policy"), [later, earlier]).unwrap();
    let mut facts = PolicyFacts::default();
    facts.add_role(role);

    let decision = set.evaluate(&request(
        "tenant:acme/app-policy",
        principal("human:alice"),
        "invoice.read",
        resource("invoice", "inv_123"),
        facts,
    ));

    assert_eq!(
        decision.reason(),
        &PolicyDecisionReason::ExplicitAllow(RuleId::new("a-allow").unwrap())
    );
}
