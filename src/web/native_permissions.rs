//! Bounded, experimental bridge to the bundled native Nulang RBAC evaluator.
//! No request values are interpolated into source or bytecode instructions.
use crate::bytecode::Constant;
use crate::mir_codegen::CompiledWebModule;
use crate::runtime::HttpMethod;
use crate::web::handler_call::{
    render_trusted_policy_handler, TypedBoundRouteArgument, TypedWebValue,
};
use crate::web::tenant_auth::{TenantRouteAuthorizer, VerifiedTenant};
use std::collections::HashMap;
use std::sync::Arc;

const SOURCE: &str = include_str!("../../packages/nulang-permissions/src/lib.nula");
// Separate trusted-policy budget (not the browser form budget). Bound the
// native evaluator's worst-case nested loop. Never truncate matching rules.
const MAX_ASSIGNMENTS: usize = 128;
const MAX_RULES: usize = 128;
const MAX_FIELD_BYTES: usize = 256;

const BRIDGE: &str = r#"
fn native_web_permission(
    tenant: String, subject: String, resource_tenant: String,
    resource: String, action: String, membership_fields: [String], rule_fields: [String]
) -> String {
    var assignments = [];
    var i = 0;
    let n = perform Array.length(membership_fields);
    while i < n {
        assignments = perform Array.push(assignments, role_assignment(
            membership_fields[i], membership_fields[i + 1], membership_fields[i + 2]
        ));
        i = i + 3
    };
    var rules = [];
    var j = 0;
    let m = perform Array.length(rule_fields);
    while j < m {
        rules = perform Array.push(rules, permission_rule(
            rule_fields[j], rule_fields[j + 1], rule_fields[j + 2],
            rule_fields[j + 3], rule_fields[j + 4]
        ));
        j = j + 5
    };
    decision_reason(tenant, subject, resource_tenant, resource, action, assignments, rules)
}
"#;

#[derive(Clone, Debug)]
pub struct RoleAssignment {
    pub tenant_id: String,
    pub subject_id: String,
    pub role: String,
}

#[derive(Clone, Debug)]
pub struct PermissionRule {
    pub tenant_id: String,
    pub role: String,
    pub resource: String,
    pub action: String,
    pub outcome: String,
}

/// One fresh, authoritative policy input. Never construct from client roles,
/// client tenant claims, or a client-selected policy revision.
pub struct PolicyInput {
    pub resource_tenant_id: String,
    pub resource: String,
    pub action: String,
    pub assignments: Vec<RoleAssignment>,
    pub rules: Vec<PermissionRule>,
}

pub type PolicyLoader = dyn Fn(&VerifiedTenant, HttpMethod, &str, &HashMap<String, String>) -> Option<PolicyInput>
    + Send
    + Sync;

/// Immutable tenant-scoped policy materialization. The authoritative loader
/// must fetch memberships, rules and revision in one consistent read and must
/// select the current revision; this value alone cannot prove freshness.
pub struct PolicySnapshot {
    tenant_id: String,
    revision: u64,
    assignments: Vec<RoleAssignment>,
    rules: Vec<PermissionRule>,
}

impl PolicySnapshot {
    pub fn new(
        tenant_id: &str,
        revision: u64,
        assignments: Vec<RoleAssignment>,
        rules: Vec<PermissionRule>,
    ) -> Result<Self, String> {
        if revision == 0 || tenant_id.is_empty() || tenant_id.len() > MAX_FIELD_BYTES {
            return Err("invalid policy snapshot tenant or revision".into());
        }
        if assignments.len() > MAX_ASSIGNMENTS || rules.len() > MAX_RULES {
            return Err("policy snapshot exceeds evaluation budget".into());
        }
        if assignments.iter().any(|a| a.tenant_id != tenant_id)
            || rules.iter().any(|r| r.tenant_id != tenant_id)
        {
            return Err("policy snapshot contains foreign tenant entries".into());
        }
        if assignments.iter().any(|a| {
            [&a.tenant_id, &a.subject_id, &a.role]
                .iter()
                .any(|s| s.len() > MAX_FIELD_BYTES)
        }) || rules.iter().any(|r| {
            [&r.tenant_id, &r.role, &r.resource, &r.action, &r.outcome]
                .iter()
                .any(|s| s.len() > MAX_FIELD_BYTES)
        }) {
            return Err("policy snapshot field exceeds evaluation budget".into());
        }
        Ok(Self {
            tenant_id: tenant_id.into(),
            revision,
            assignments,
            rules,
        })
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }
}

pub struct SnapshotPolicyInput {
    pub resource_tenant_id: String,
    pub resource: String,
    pub action: String,
    pub snapshot: Arc<PolicySnapshot>,
}

pub type SnapshotPolicyLoader = dyn Fn(&VerifiedTenant, HttpMethod, &str, &HashMap<String, String>) -> Option<SnapshotPolicyInput>
    + Send
    + Sync;

pub struct NativePermissions {
    compiled: CompiledWebModule,
    function_index: usize,
}

impl NativePermissions {
    /// Compile the bundled package, not application or request-supplied code.
    pub fn compile() -> Result<Self, String> {
        let source = format!("{SOURCE}\n{BRIDGE}");
        let tokens = crate::lexer::Lexer::new(&source)
            .lex()
            .map_err(|e| format!("{e:?}"))?;
        let ast = crate::parser::Parser::new(tokens)
            .parse_module()
            .map_err(|e| format!("{e:?}"))?;
        let mut checker = crate::typechecker::TypeChecker::new();
        checker.check_module(&ast).map_err(|e| format!("{e:?}"))?;
        let hir = crate::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir).map_err(|e| format!("{e:?}"))?;
        let compiled = crate::mir_codegen::compile_mir_with_web_field_layout(
            &mut mir,
            "native-web-permissions",
        )
        .map_err(|e| format!("{e:?}"))?;
        let offset = compiled
            .module()
            .debug_functions
            .iter()
            .find(|f| f.name == "native_web_permission")
            .ok_or("missing native permissions entry point")?
            .code_offset;
        let function_index = compiled
            .module()
            .function_table
            .iter()
            .position(|value| *value == offset)
            .ok_or("missing native permissions function index")?;
        Ok(Self {
            compiled,
            function_index,
        })
    }

    /// Loader is trusted to resolve canonical resource ownership and fresh
    /// membership/policy state. None, oversized inputs, VM errors and every
    /// result other than the native evaluator's exact allow decision deny.
    pub fn authorizer(self, loader: Arc<PolicyLoader>) -> Arc<TenantRouteAuthorizer> {
        Arc::new(move |principal, method, route, params| {
            loader(principal, method, route, params)
                .is_some_and(|input| self.permits(principal, &input))
        })
    }

    /// Load a single immutable policy revision for this request. Never cache
    /// a revision here: the loader owns freshness and revocation semantics.
    pub fn snapshot_authorizer(
        self,
        loader: Arc<SnapshotPolicyLoader>,
    ) -> Arc<TenantRouteAuthorizer> {
        Arc::new(move |principal, method, route, params| {
            loader(principal, method, route, params).is_some_and(|input| {
                input.snapshot.tenant_id() == principal.tenant_id()
                    && self.evaluate(
                        principal,
                        &input.resource_tenant_id,
                        &input.resource,
                        &input.action,
                        &input.snapshot.assignments,
                        &input.snapshot.rules,
                    )
            })
        })
    }

    fn permits(&self, principal: &VerifiedTenant, input: &PolicyInput) -> bool {
        self.evaluate(
            principal,
            &input.resource_tenant_id,
            &input.resource,
            &input.action,
            &input.assignments,
            &input.rules,
        )
    }

    fn evaluate(
        &self,
        principal: &VerifiedTenant,
        resource_tenant_id: &str,
        resource: &str,
        action: &str,
        assignments: &[RoleAssignment],
        rules: &[PermissionRule],
    ) -> bool {
        if assignments.len() > MAX_ASSIGNMENTS || rules.len() > MAX_RULES {
            return false;
        }
        let scalars = [
            principal.tenant_id(),
            principal.subject_id(),
            resource_tenant_id,
            resource,
            action,
        ];
        let assignments: Vec<&str> = assignments
            .iter()
            .flat_map(|a| [a.tenant_id.as_str(), a.subject_id.as_str(), a.role.as_str()])
            .collect();
        let rules: Vec<&str> = rules
            .iter()
            .flat_map(|r| {
                [
                    r.tenant_id.as_str(),
                    r.role.as_str(),
                    r.resource.as_str(),
                    r.action.as_str(),
                    r.outcome.as_str(),
                ]
            })
            .collect();
        if scalars
            .iter()
            .chain(&assignments)
            .chain(&rules)
            .any(|field| field.len() > MAX_FIELD_BYTES)
        {
            return false;
        }
        let constant = |s: &str| Constant::String(s.to_owned());
        let mut args: Vec<_> = scalars
            .iter()
            .enumerate()
            .map(|(handler_index, value)| TypedBoundRouteArgument {
                handler_index,
                value: TypedWebValue::Scalar(constant(value)),
            })
            .collect();
        for (handler_index, fields) in [(5, assignments), (6, rules)] {
            args.push(TypedBoundRouteArgument {
                handler_index,
                value: TypedWebValue::ArrayScalars(fields.into_iter().map(constant).collect()),
            });
        }
        render_trusted_policy_handler(&self.compiled, self.function_index, 7, &args)
            .is_ok_and(|decision| decision == "allow")
    }
}
