//! Bounded, experimental bridge to the bundled native Nulang RBAC evaluator.
//! No request values are interpolated into source or bytecode instructions.
use crate::bytecode::Constant;
use crate::mir_codegen::CompiledWebModule;
use crate::runtime::HttpMethod;
use crate::web::handler_call::{
    render_typed_bound_handler, TypedBoundRouteArgument, TypedWebValue,
};
use crate::web::tenant_auth::{TenantRouteAuthorizer, VerifiedTenant};
use std::collections::HashMap;
use std::sync::Arc;

const SOURCE: &str = include_str!("../../packages/nulang-permissions/src/lib.nula");
// The existing typed VM ABI caps scalar arrays at 64 entries. Never truncate:
// omission of a deny rule can turn a denial into an allow.
const MAX_ASSIGNMENTS: usize = 64 / 3;
const MAX_RULES: usize = 64 / 5;
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

    fn permits(&self, principal: &VerifiedTenant, input: &PolicyInput) -> bool {
        if input.assignments.len() > MAX_ASSIGNMENTS || input.rules.len() > MAX_RULES {
            return false;
        }
        let scalars = [
            principal.tenant_id(),
            principal.subject_id(),
            &input.resource_tenant_id,
            &input.resource,
            &input.action,
        ];
        let assignments: Vec<&str> = input
            .assignments
            .iter()
            .flat_map(|a| [a.tenant_id.as_str(), a.subject_id.as_str(), a.role.as_str()])
            .collect();
        let rules: Vec<&str> = input
            .rules
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
        render_typed_bound_handler(&self.compiled, self.function_index, 7, &args)
            .is_ok_and(|decision| decision == "allow")
    }
}
