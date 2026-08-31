//! AST -> HIR lowering.
//!
//! Converts the parsed, type-checked AST into the typed High-level IR.
//! Expression types fall back to `Type::unit()` when no explicit annotation
//! is available; the bytecode backend is dynamically typed, so structural
//! fidelity (not type fidelity) is what matters here.
//!
//! Control flow in expression position (`if`, `match`, `for`) lowers to
//! dedicated `RValue` variants whose sub-bodies end in a `Yield` terminator.
//! This keeps evaluation order correct when statements follow the control
//! flow expression — the old design stored `if` as a *body terminator*,
//! which reordered any code lowered after it.

use crate::ast;
use crate::ast::{BinOp, Decl, Expr, FunctionAnnotation, Literal};
use crate::hir;
use crate::tool_schema::{function_to_tool_schema, ToolSchema};
use crate::types::{Capability, EffectRow, Span, Type, TypeVar};

type FxHashMap<K, V> =
    std::collections::HashMap<K, V, std::hash::BuildHasherDefault<rustc_hash::FxHasher>>;

pub fn lower_module(
    ast: &ast::AstModule,
    inferred_decl_types: &FxHashMap<String, Type>,
) -> hir::Module {
    let mut module = hir::Module::new(&ast.name);
    let tools = collect_tool_schemas(&ast.decls);

    // Scan for given declarations, using params, and dict params.
    let mut givens = FxHashMap::default();
    let mut fn_using = FxHashMap::default();
    let mut fn_dict_params: FxHashMap<String, Vec<(String, TypeVar, Vec<String>)>> =
        FxHashMap::default();
    for decl in &ast.decls {
        if let Decl::Given { name, value, .. } = decl {
            givens.insert(name.clone(), value.clone());
        }
        if let Decl::Function {
            name,
            using_params,
            type_param_constraints,
            ..
        } = decl
        {
            if !using_params.is_empty() {
                fn_using.insert(
                    name.clone(),
                    using_params.iter().map(|p| p.name.clone()).collect(),
                );
            }
            if !type_param_constraints.is_empty() {
                fn_dict_params.insert(name.clone(), type_param_constraints.clone());
            }
        }
    }
    GIVEN_BINDINGS.with(|c| *c.borrow_mut() = givens);
    FN_USING_PARAMS.with(|c| *c.borrow_mut() = fn_using);
    FN_DICT_PARAMS.with(|c| *c.borrow_mut() = fn_dict_params);

    let signal_inits: FxHashMap<String, Expr> = ast
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Signal { name, init, .. } => Some((name.clone(), init.clone())),
            _ => None,
        })
        .collect();
    SIGNAL_INITS.with(|c| *c.borrow_mut() = signal_inits);

    let class_tables = crate::typechecker::build_class_tables(ast);
    CURRENT_CLASS_TABLES.with(|cell| {
        *cell.borrow_mut() = Some(class_tables);
    });
    CURRENT_INFERRED_DECL_TYPES.with(|cell| {
        *cell.borrow_mut() = Some(inferred_decl_types.clone());
    });

    for decl in &ast.decls {
        if matches!(decl, Decl::NamedHandler { .. } | Decl::Class { .. }) {
            continue;
        }
        module.decls.push(lower_decl(decl, &tools));
    }

    CURRENT_CLASS_TABLES.with(|cell| {
        *cell.borrow_mut() = None;
    });
    CURRENT_INFERRED_DECL_TYPES.with(|cell| {
        *cell.borrow_mut() = None;
    });

    module
}

/// Collect `@tool`-annotated function signatures across the whole module
/// (including nested modules), mirroring the stable compiler's
/// `collect_functions` so `agent` declarations can resolve `tools: [...]`
/// names regardless of source order.
fn collect_tool_schemas(decls: &[Decl]) -> Vec<ToolSchema> {
    let mut tools = Vec::new();
    collect_tool_schemas_into(decls, &mut tools);
    tools
}

fn collect_tool_schemas_into(decls: &[Decl], tools: &mut Vec<ToolSchema>) {
    for decl in decls {
        match decl {
            Decl::Function {
                name,
                params,
                ret_type,
                annotations,
                ..
            } if name != "__main" => {
                if let Some(FunctionAnnotation::Tool { description }) = annotations
                    .iter()
                    .find(|a| matches!(a, FunctionAnnotation::Tool { .. }))
                {
                    let mut typed_params = Vec::with_capacity(params.len());
                    let mut all_typed = true;
                    for p in params {
                        if let Some(ty) = &p.ty {
                            typed_params.push((p.name.clone(), ty.clone()));
                        } else {
                            all_typed = false;
                            break;
                        }
                    }
                    if all_typed {
                        let ret = ret_type.clone().unwrap_or_else(Type::unit);
                        tools.push(function_to_tool_schema(
                            name,
                            description,
                            &typed_params,
                            &ret,
                        ));
                    }
                }
            }
            Decl::Module {
                decls: subdecls, ..
            } => collect_tool_schemas_into(subdecls, tools),
            _ => {}
        }
    }
}

fn lower_decl(decl: &Decl, tools: &[ToolSchema]) -> hir::Decl {
    match decl {
        Decl::CrdtDecl {
            name,
            fields: _,
            span,
        } => {
            // In a full implementation, this would create a CRDT actor
            hir::Decl::Constant {
                name: name.clone(),
                body: hir::Body {
                    stmts: vec![],
                    terminator: hir::Terminator::FnReturn(Some(hir::Operand::Unit)),
                },
                span: *span,
            }
        }
        Decl::Function {
            name,
            type_params,
            type_param_constraints,
            params,
            using_params,
            ret_type,
            error_type: _,
            effect,
            cap,
            body,
            annotations,
            public,
            span,
            ..
        } => {
            let mut all_params: Vec<(String, Type)> = params
                .iter()
                .map(|p| (p.name.clone(), resolve_type(&p.ty)))
                .collect();
            for p in using_params {
                all_params.push((p.name.clone(), resolve_type(&p.ty)));
            }
            // Build implicit dictionary parameters from typeclass constraints.
            let dict_param_names: Vec<String> = type_param_constraints
                .iter()
                .flat_map(|(tp, _, cs)| cs.iter().map(move |c| format!("_dict_{}_{}", c, tp)))
                .collect();
            for dn in &dict_param_names {
                all_params.push((dn.clone(), Type::unit()));
            }
            // Build param map for typeclass resolution during body lowering.
            let mut param_map: FxHashMap<String, Type> = FxHashMap::default();
            for (n, t) in &all_params {
                param_map.insert(n.clone(), t.clone());
            }
            let explicit_placement = annotations.iter().find_map(|a| match a {
                crate::ast::FunctionAnnotation::Placement(p) => Some(*p),
                _ => None,
            });
            // Default placement inference from declared effect row (only when the
            // user explicitly declared effects; inferred rows remain None here).
            let inferred_placement =
                explicit_placement.or_else(|| {
                    let row = effect.as_ref()?;
                    let effects: Vec<_> = match row {
                        crate::types::EffectRow::Closed(effs) => effs.clone(),
                        crate::types::EffectRow::Open(effs, _) => effs.clone(),
                    };
                    if effects.iter().any(|e| {
                        *e == crate::types::Effect::Request || *e == crate::types::Effect::Web
                    }) {
                        Some(crate::types::Placement::Server)
                    } else if effects.iter().all(|e| {
                        *e == crate::types::Effect::Render || *e == crate::types::Effect::Web
                    }) && !effects.is_empty()
                    {
                        Some(crate::types::Placement::Static)
                    } else {
                        None
                    }
                });
            hir::Decl::Function(hir::FunctionDef {
                name: name.clone(),
                type_params: type_params.clone(),
                params: all_params,
                dict_params: dict_param_names
                    .into_iter()
                    .map(|n| (n, Type::unit()))
                    .collect(),
                ret: resolve_type(ret_type),
                effect: effect.clone().unwrap_or_else(EffectRow::empty),
                cap: cap.unwrap_or(Capability::Ref),
                body: {
                    CURRENT_TYPE_PARAM_CONSTRAINTS
                        .with(|c| *c.borrow_mut() = type_param_constraints.clone());
                    CURRENT_FN_PARAMS.with(|c| *c.borrow_mut() = param_map);
                    let b = with_fresh_defer_stack(|| lower_body(body));
                    CURRENT_TYPE_PARAM_CONSTRAINTS.with(|c| *c.borrow_mut() = Vec::new());
                    CURRENT_FN_PARAMS.with(|c| *c.borrow_mut() = FxHashMap::default());
                    b
                },
                public: *public,
                placement: inferred_placement,
                span: *span,
            })
        }
        Decl::Actor {
            name,
            type_params,
            persistent,
            state_fields,
            behaviors,
            init,
            virtual_,
            events,
            apply_handlers,
            version,
            migrations,
            is_organization,
            span,
            ..
        } => hir::Decl::Actor(hir::ActorDef {
            name: name.clone(),
            type_params: type_params.clone(),
            persistent: *persistent,
            state_fields: state_fields
                .iter()
                .map(|(n, m, t, e)| {
                    let mut body = hir::Body::new();
                    let op = lower_expr(e, &mut body);
                    (n.clone(), *m, t.clone(), op)
                })
                .collect(),
            behaviors: behaviors
                .iter()
                .map(|b| lower_behavior(b, apply_handlers))
                .collect(),
            init: init
                .iter()
                .map(|(n, e)| {
                    let mut body = hir::Body::new();
                    let op = lower_expr(e, &mut body);
                    (n.clone(), op)
                })
                .collect(),
            events: events.clone(),
            apply_handlers: apply_handlers.clone(),
            version: *version,
            migrations: migrations.clone(),
            is_workflow: false,
            is_organization: *is_organization,
            is_agent: false,
            virtual_: *virtual_,
            tools: Vec::new(),
            semantic_memory_dimensions: None,
            procedural_memory_namespace: None,
            fallback_config: String::new(),
            retry_config: String::new(),
            span: *span,
        }),
        Decl::TypeAlias {
            name,
            type_params,
            body,
            opaque,
            public,
            span,
        } => hir::Decl::TypeAlias {
            name: name.clone(),
            type_params: type_params.clone(),
            body: body.clone(),
            opaque: *opaque,
            public: *public,
            span: *span,
        },
        Decl::RecordType {
            name,
            type_params,
            fields,
            public,
            span,
            ..
        } => hir::Decl::RecordType {
            name: name.clone(),
            type_params: type_params.clone(),
            fields: fields.clone(),
            public: *public,
            span: *span,
        },
        Decl::VariantType {
            name,
            type_params,
            variants,
            public,
            span,
        } => hir::Decl::VariantType {
            name: name.clone(),
            type_params: type_params.clone(),
            variants: variants.clone(),
            public: *public,
            span: *span,
        },
        Decl::EffectDecl { name, ops, span } => hir::Decl::EffectDecl {
            name: name.clone(),
            ops: ops.clone(),
            span: *span,
        },
        Decl::Extern {
            library,
            funcs,
            span,
        } => hir::Decl::ExternBlock {
            library: library.clone(),
            funcs: funcs
                .iter()
                .map(|f| hir::ExternFunc {
                    name: f.name.clone(),
                    params: f
                        .params
                        .iter()
                        .map(|(n, t)| (n.clone(), t.clone()))
                        .collect(),
                    ret: f.ret.clone(),
                    span: f.span,
                })
                .collect(),
            span: *span,
        },
        Decl::Module {
            name,
            exports,
            decls,
            span,
        } => hir::Decl::Module {
            name: name.clone(),
            exports: exports.clone(),
            decls: decls.iter().map(|d| lower_decl(d, tools)).collect(),
            span: *span,
        },
        Decl::Import { path, items, span } => hir::Decl::Import {
            path: path.clone(),
            items: items.clone(),
            span: *span,
        },

        Decl::LetBinding {
            name, value, span, ..
        } => {
            let mut body = hir::Body::new();
            let op = lower_expr(value, &mut body);
            body.set_terminator(hir::Terminator::FnReturn(Some(op)));
            hir::Decl::Constant {
                name: name.clone(),
                body,
                span: *span,
            }
        }
        Decl::Signal { .. } => {
            // Signals are compile-time metadata for the reactivity pass and are
            // inlined at use sites in `lower_expr`. They produce no HIR decl.
            return hir::Decl::Import {
                path: String::new(),
                items: Vec::new(),
                span: Span::default(),
            };
        }
        Decl::Workflow {
            name, items, span, ..
        } => desugar_workflow(name, items, *span),
        Decl::StateMachine {
            name,
            states,
            events,
            entry_hooks,
            exit_hooks,
            span,
        } => {
            // Desugar to an ordinary actor declaration, then lower it
            // through the standard actor path (mirrors desugar_workflow,
            // which also targets hir::Decl::Actor).
            lower_decl(
                &ast::desugar_state_machine(name, states, events, entry_hooks, exit_hooks, *span),
                tools,
            )
        }
        Decl::Impl {
            class_name,
            for_type,
            methods,
            span,
            ..
        } => {
            let mut body = hir::Body::new();
            let mut fields: Vec<(String, hir::Operand)> = Vec::new();
            for method in methods {
                let lambda_params: Vec<crate::ast::Param> = method
                    .params
                    .iter()
                    .map(|(n, t)| crate::ast::Param {
                        name: n.clone(),
                        ty: Some(t.clone()),
                        cap: None,
                    })
                    .collect();
                let lambda = Expr::Lambda {
                    params: lambda_params,
                    ret_type: Some(method.return_type.clone()),
                    effect: None,
                    body: Box::new(method.body.clone()),
                    span: Span::default(),
                };
                let op = lower_expr(&lambda, &mut body);
                fields.push((method.name.clone(), op));
            }
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: Type::unit(),
                value: hir::RValue::Record(fields, Type::unit()),
                span: Span::default(),
            });
            body.set_terminator(hir::Terminator::FnReturn(Some(hir::Operand::Var(
                temp,
                Type::unit(),
            ))));
            let dict_name = format!("_impl_{}_{}", class_name, for_type);
            hir::Decl::Constant {
                name: dict_name,
                body,
                span: *span,
            }
        }
        Decl::Class { .. } => {
            // Class is filtered out in lower_module before reaching
            // lower_decl; this arm exists only for exhaustiveness.
            unreachable!("Class should be filtered by lower_module")
        }
        Decl::Agent {
            name,
            model,
            system_prompt,
            tools: tool_names,
            memory,
            semantic_memory,
            procedural_memory,
            pricing,
            fallback,
            retry,
            span,
        } => desugar_agent(
            name,
            model,
            system_prompt,
            tool_names,
            memory,
            semantic_memory,
            procedural_memory,
            pricing,
            fallback,
            retry,
            tools,
            *span,
        ),
        Decl::NamedHandler { .. } => {
            // NamedHandler is filtered out in lower_module before reaching
            // lower_decl; this arm exists only for exhaustiveness.
            unreachable!("NamedHandler should be filtered by lower_module")
        }
        Decl::Database { name, tables, span } => hir::Decl::Database {
            name: name.clone(),
            tables: tables.clone(),
            span: *span,
        },
        Decl::Given { span, .. } => {
            // Given declarations are resolved to call-site arguments
            // during typechecking and do not produce HIR nodes.
            hir::Decl::Constant {
                name: "_unused_given".to_string(),
                body: hir::Body::new(),
                span: *span,
            }
        }
    }
}

fn lower_behavior(b: &ast::Behavior, apply_handlers: &[ast::ApplyHandler]) -> hir::BehaviorDef {
    // Set thread-local apply handlers so lower_expr can inject apply code after emit
    if !apply_handlers.is_empty() {
        CURRENT_APPLY_HANDLERS.with(|cell| {
            *cell.borrow_mut() = Some(apply_handlers.to_vec());
        });
    }
    let body = with_fresh_defer_stack(|| lower_body(&b.body));
    CURRENT_APPLY_HANDLERS.with(|cell| {
        *cell.borrow_mut() = None;
    });
    hir::BehaviorDef {
        name: b.name.clone(),
        params: b
            .params
            .iter()
            .map(|p| (p.name.clone(), resolve_type(&p.ty)))
            .collect(),
        ret: Type::unit(),
        effect: b.effect.clone().unwrap_or_else(EffectRow::empty),
        cap: b.cap,
        body,
        compensate: None,
        parallel_branches: None,
        span: b.span,
    }
}

/// Placeholder behavior for a memory operation the runtime intercepts by
/// name (see `is_agent`/`semantic_memory_dimensions`/
/// `procedural_memory_namespace` on `hir::ActorDef`) instead of running its
/// bytecode body.
fn placeholder_behavior(name: &str, params: Vec<(&str, Type)>, span: Span) -> ast::Behavior {
    ast::Behavior {
        name: name.to_string(),
        params: params
            .into_iter()
            .map(|(n, t)| crate::ast::Param {
                name: n.to_string(),
                ty: Some(t),
                cap: None,
            })
            .collect(),
        body: Expr::Literal(Literal::Unit, span),
        effect: None,
        cap: Capability::Ref,
        ret_type: None,
        span,
    }
}

/// Desugar an `agent Name = { ... }` declaration into an actor: durable
/// state fields hold the model/prompt/memory configuration, and generated
/// behaviors implement `ask`/`usage` (plus memory operations, intercepted by
/// the runtime rather than executed as bytecode). Mirrors the stable
/// compiler's `compile_agent` exactly, so both backends produce the same
/// source-level shape — synthesized `ast::Behavior` bodies are lowered
/// through the ordinary `lower_behavior`/`lower_expr` path.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(feature = "ai-runtime"), allow(unused_variables))]
fn desugar_agent(
    name: &str,
    model: &str,
    system_prompt: &Option<String>,
    tool_names: &[String],
    memory: &Option<ast::AgentMemoryConfig>,
    semantic_memory: &Option<ast::AgentSemanticMemoryConfig>,
    procedural_memory: &Option<ast::AgentProceduralMemoryConfig>,
    pricing: &Option<ast::AgentPricing>,
    fallback: &[ast::AgentFallbackEntry],
    retry: &Option<ast::AgentRetryConfig>,
    available_tools: &[ToolSchema],
    span: Span,
) -> hir::Decl {
    // Resolve tool names against the module's @tool-annotated functions,
    // mirroring the stable compiler's `compile_agent`. An unresolvable name
    // means the whole program is invalid; the typechecker / stable compiler
    // is responsible for raising the "unknown tool" error. We still produce
    // a well-formed actor so HIR→MIR lowering never sees a bare Agent decl.
    let mut resolved_tools = Vec::with_capacity(tool_names.len());
    for tool_name in tool_names {
        if let Some(schema) = available_tools.iter().find(|t| &t.name == tool_name) {
            resolved_tools.push(schema.clone());
        }
    }

    let agent_pricing = pricing.unwrap_or(ast::AgentPricing {
        input: 0.0,
        output: 0.0,
    });
    #[cfg(feature = "ai-runtime")]
    let max_turns = memory.as_ref().map(|m| m.max_turns).unwrap_or(50);
    #[cfg(feature = "ai-runtime")]
    let initial_memory = serde_json::to_string(&nulang_ai::EpisodicMemory::new(max_turns))
        .unwrap_or_else(|_| "{}".to_string());
    #[cfg(not(feature = "ai-runtime"))]
    let initial_memory = "{}".to_string();

    let semantic_memory_dimensions = semantic_memory.as_ref().map(|m| m.dimensions);
    let initial_semantic_memory = semantic_memory_dimensions.map(|dimensions| {
        #[cfg(feature = "ai-runtime")]
        {
            serde_json::to_string(&nulang_ai::SemanticMemory::new(dimensions, None))
                .unwrap_or_else(|_| "{}".to_string())
        }
        #[cfg(not(feature = "ai-runtime"))]
        {
            let _ = dimensions;
            "{}".to_string()
        }
    });

    let procedural_memory_namespace = procedural_memory.as_ref().map(|m| m.namespace.clone());
    let initial_procedural_memory = procedural_memory_namespace.as_ref().map(|namespace| {
        #[cfg(feature = "ai-runtime")]
        {
            serde_json::to_string(&nulang_ai::ProceduralMemory::new(namespace.clone()))
                .unwrap_or_else(|_| "{}".to_string())
        }
        #[cfg(not(feature = "ai-runtime"))]
        {
            let _ = namespace;
            "{}".to_string()
        }
    });

    let str_ty = Type::string();
    let int_ty = Type::int();
    let float_ty = Type::float();
    let lit_op = |lit: Literal, ty: Type| hir::Operand::Literal(lit, ty);

    let mut state_fields: Vec<(String, ast::StateModel, Type, hir::Operand)> = vec![
        (
            "model".to_string(),
            ast::StateModel::Durable,
            str_ty.clone(),
            lit_op(Literal::String(model.to_string()), str_ty.clone()),
        ),
        (
            "system_prompt".to_string(),
            ast::StateModel::Durable,
            str_ty.clone(),
            lit_op(
                Literal::String(system_prompt.clone().unwrap_or_default()),
                str_ty.clone(),
            ),
        ),
        (
            "episodic_memory".to_string(),
            ast::StateModel::Durable,
            str_ty.clone(),
            lit_op(Literal::String(initial_memory), str_ty.clone()),
        ),
        (
            "usage_prompt".to_string(),
            ast::StateModel::Durable,
            int_ty.clone(),
            lit_op(Literal::Int(0), int_ty.clone()),
        ),
        (
            "usage_completion".to_string(),
            ast::StateModel::Durable,
            int_ty.clone(),
            lit_op(Literal::Int(0), int_ty.clone()),
        ),
        (
            "usage_cost".to_string(),
            ast::StateModel::Durable,
            float_ty.clone(),
            lit_op(Literal::Float(0.0), float_ty.clone()),
        ),
        (
            "pricing_input".to_string(),
            ast::StateModel::Durable,
            float_ty.clone(),
            lit_op(Literal::Float(agent_pricing.input), float_ty.clone()),
        ),
        (
            "pricing_output".to_string(),
            ast::StateModel::Durable,
            float_ty.clone(),
            lit_op(Literal::Float(agent_pricing.output), float_ty.clone()),
        ),
    ];
    if let Some(json) = initial_semantic_memory {
        state_fields.push((
            "semantic_memory".to_string(),
            ast::StateModel::Durable,
            str_ty.clone(),
            lit_op(Literal::String(json), str_ty.clone()),
        ));
    }
    if let Some(json) = initial_procedural_memory {
        state_fields.push((
            "procedural_memory".to_string(),
            ast::StateModel::Durable,
            str_ty.clone(),
            lit_op(Literal::String(json), str_ty.clone()),
        ));
    }

    // Serialize fallback config into a durable JSON string.
    let fallback_config_json =
        serde_json::to_string(&fallback).unwrap_or_else(|_| "[]".to_string());
    state_fields.push((
        "fallback_config".to_string(),
        ast::StateModel::Durable,
        str_ty.clone(),
        lit_op(Literal::String(fallback_config_json), str_ty.clone()),
    ));

    // Serialize retry config into a durable JSON string.
    let retry_config_json = serde_json::to_string(&retry).unwrap_or_else(|_| "null".to_string());
    state_fields.push((
        "retry_config".to_string(),
        ast::StateModel::Durable,
        str_ty.clone(),
        lit_op(Literal::String(retry_config_json), str_ty.clone()),
    ));

    // Tracking fields for the retry/fallback state machine.
    state_fields.push((
        "llm_attempt".to_string(),
        ast::StateModel::Durable,
        int_ty.clone(),
        lit_op(Literal::Int(0), int_ty.clone()),
    ));
    state_fields.push((
        "llm_fallback_step".to_string(),
        ast::StateModel::Durable,
        int_ty.clone(),
        lit_op(Literal::Int(0), int_ty.clone()),
    ));

    // Generated ask behavior reads agent state and performs the LLM ask.
    let ask_behavior = ast::Behavior {
        name: "ask".to_string(),
        params: vec![crate::ast::Param {
            name: "prompt".to_string(),
            ty: Some(str_ty.clone()),
            cap: None,
        }],
        body: Expr::Block {
            exprs: vec![
                Expr::FieldAccess {
                    expr: Box::new(Expr::SelfRef(span)),
                    field: "model".to_string(),
                    span,
                },
                Expr::FieldAccess {
                    expr: Box::new(Expr::SelfRef(span)),
                    field: "system_prompt".to_string(),
                    span,
                },
                Expr::FieldAccess {
                    expr: Box::new(Expr::SelfRef(span)),
                    field: "episodic_memory".to_string(),
                    span,
                },
                Expr::Perform {
                    effect: "Inference".to_string(),
                    op: "ask".to_string(),
                    args: vec![Expr::Var("prompt".to_string(), span)],
                    span,
                },
            ],
            span,
        },
        effect: None,
        cap: Capability::Ref,
        ret_type: None,
        span,
    };

    // Generated usage behavior returns cumulative usage/cost state as a
    // plain array [prompt_tokens, completion_tokens, cost].
    let usage_behavior = ast::Behavior {
        name: "usage".to_string(),
        params: vec![],
        body: Expr::Array(
            vec![
                Expr::FieldAccess {
                    expr: Box::new(Expr::SelfRef(span)),
                    field: "usage_prompt".to_string(),
                    span,
                },
                Expr::FieldAccess {
                    expr: Box::new(Expr::SelfRef(span)),
                    field: "usage_completion".to_string(),
                    span,
                },
                Expr::FieldAccess {
                    expr: Box::new(Expr::SelfRef(span)),
                    field: "usage_cost".to_string(),
                    span,
                },
            ],
            span,
        ),
        effect: None,
        cap: Capability::Ref,
        ret_type: None,
        span,
    };

    let mut behaviors = vec![
        lower_behavior(&ask_behavior, &[]),
        lower_behavior(&usage_behavior, &[]),
    ];

    if semantic_memory_dimensions.is_some() {
        behaviors.push(lower_behavior(
            &placeholder_behavior("store_fact", vec![("content", str_ty.clone())], span),
            &[],
        ));
        behaviors.push(lower_behavior(
            &placeholder_behavior(
                "recall",
                vec![("query", str_ty.clone()), ("top_k", int_ty.clone())],
                span,
            ),
            &[],
        ));
    }
    if procedural_memory_namespace.is_some() {
        behaviors.push(lower_behavior(
            &placeholder_behavior(
                "store_pattern",
                vec![
                    ("key", str_ty.clone()),
                    ("input_pattern", str_ty.clone()),
                    ("output_template", str_ty.clone()),
                ],
                span,
            ),
            &[],
        ));
        behaviors.push(lower_behavior(
            &placeholder_behavior("get_pattern", vec![("key", str_ty.clone())], span),
            &[],
        ));
        behaviors.push(lower_behavior(
            &placeholder_behavior(
                "add_example",
                vec![
                    ("task", str_ty.clone()),
                    ("input", str_ty.clone()),
                    ("output", str_ty.clone()),
                ],
                span,
            ),
            &[],
        ));
        behaviors.push(lower_behavior(
            &placeholder_behavior(
                "get_examples",
                vec![
                    ("task", str_ty.clone()),
                    ("query", str_ty.clone()),
                    ("top_k", int_ty.clone()),
                ],
                span,
            ),
            &[],
        ));
    }

    // Already serialized above for state fields; reuse for ActorDef metadata.
    let fallback_config_str = serde_json::to_string(&fallback).unwrap_or_else(|_| "[]".to_string());
    let retry_config_str = serde_json::to_string(&retry).unwrap_or_else(|_| "null".to_string());

    hir::Decl::Actor(hir::ActorDef {
        name: name.to_string(),
        type_params: Vec::new(),
        persistent: true,
        state_fields,
        behaviors,
        init: Vec::new(),
        events: Vec::new(),
        apply_handlers: Vec::new(),
        version: 1,
        migrations: Vec::new(),
        is_workflow: false,
        is_organization: false,
        is_agent: true,
        virtual_: false,
        tools: resolved_tools,
        semantic_memory_dimensions,
        procedural_memory_namespace,
        fallback_config: fallback_config_str,
        retry_config: retry_config_str,
        span,
    })
}

/// Desugar a `workflow Name { step ... }` declaration into a persistent
/// actor: one behavior per step, plus a durable `step_index` counter the
/// runtime advances as steps complete. Mirrors the stable compiler's
/// `compile_workflow` for the sequential case; a workflow containing a
/// `parallel` block falls back honestly (parallel-branch synthesis and its
/// progress-counter bookkeeping is a separate, not-yet-ported effort).
fn desugar_workflow(name: &str, items: &[ast::WorkflowItem], span: Span) -> hir::Decl {
    // Flatten the ordered workflow items into a list of sequential steps.
    // Each `parallel` block becomes a synthetic step whose body runs
    // branches sequentially (guarded by a durable `parallel_progress`
    // counter so recovery skips branches that already completed before a
    // crash) and emits a `ParallelBranchCompleted` event after each branch.
    // Mirrors the stable compiler's `compile_workflow` exactly.
    let mut flattened_steps: Vec<ast::WorkflowStep> = Vec::new();
    let mut parallel_branch_names: FxHashMap<usize, Vec<String>> = FxHashMap::default();
    let mut parallel_counter = 0usize;

    for item in items {
        match item {
            ast::WorkflowItem::Step(step) => flattened_steps.push(step.clone()),
            ast::WorkflowItem::Parallel(branches) => {
                let parallel_name = format!("parallel_{}", parallel_counter);
                parallel_counter += 1;

                let progress_expr = Expr::FieldAccess {
                    expr: Box::new(Expr::SelfRef(span)),
                    field: "parallel_progress".to_string(),
                    span,
                };
                let mut body_exprs: Vec<Expr> = Vec::with_capacity(branches.len() + 1);
                for (branch_idx, branch) in branches.iter().enumerate() {
                    let threshold = (branch_idx + 1) as i64;
                    let guard = Expr::Binary {
                        op: BinOp::Lt,
                        left: Box::new(progress_expr.clone()),
                        right: Box::new(Expr::Literal(Literal::Int(threshold), span)),
                        span,
                    };
                    let branch_block = Expr::Block {
                        exprs: vec![
                            branch.body.clone(),
                            Expr::Emit {
                                event: "ParallelBranchCompleted".to_string(),
                                args: vec![
                                    Expr::Literal(Literal::String(parallel_name.clone()), span),
                                    Expr::Literal(Literal::String(branch.name.clone()), span),
                                ],
                                span,
                            },
                        ],
                        span,
                    };
                    body_exprs.push(Expr::If {
                        cond: Box::new(guard),
                        then_branch: Box::new(branch_block),
                        else_branch: None,
                        span,
                    });
                }
                // Reset the parallel-progress counter once every branch has
                // finished. The runtime advances step_index when it records
                // StepCompleted so signal-waiting branches don't
                // double-increment.
                body_exprs.push(Expr::Assign {
                    target: Box::new(progress_expr.clone()),
                    value: Box::new(Expr::Literal(Literal::Int(0), span)),
                    span,
                });

                let combined_compensate = {
                    let comp_exprs: Vec<Expr> = branches
                        .iter()
                        .rev()
                        .filter_map(|b| b.compensate.clone())
                        .collect();
                    if comp_exprs.is_empty() {
                        None
                    } else {
                        Some(Expr::Block {
                            exprs: comp_exprs,
                            span,
                        })
                    }
                };

                flattened_steps.push(ast::WorkflowStep {
                    name: parallel_name.clone(),
                    body: Expr::Block {
                        exprs: body_exprs,
                        span,
                    },
                    compensate: combined_compensate,
                    span,
                });
                parallel_branch_names.insert(
                    flattened_steps.len() - 1,
                    branches.iter().map(|b| b.name.clone()).collect(),
                );
            }
        }
    }

    let state_fields: Vec<(String, ast::StateModel, Type, hir::Operand)> = vec![
        (
            "step_index".to_string(),
            ast::StateModel::Durable,
            Type::int(),
            hir::Operand::Literal(Literal::Int(0), Type::int()),
        ),
        (
            "workflow_name".to_string(),
            ast::StateModel::Durable,
            Type::string(),
            hir::Operand::Literal(Literal::String(name.to_string()), Type::string()),
        ),
        (
            "parallel_progress".to_string(),
            ast::StateModel::Durable,
            Type::int(),
            hir::Operand::Literal(Literal::Int(0), Type::int()),
        ),
    ];

    let behaviors = flattened_steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut def = lower_behavior(
                &ast::Behavior {
                    name: s.name.clone(),
                    params: Vec::new(),
                    body: s.body.clone(),
                    effect: None,
                    cap: Capability::Ref,
                    ret_type: None,
                    span: s.span,
                },
                &[],
            );
            def.compensate = s.compensate.as_ref().map(lower_body);
            def.parallel_branches = parallel_branch_names.get(&i).cloned();
            def
        })
        .collect();

    hir::Decl::Actor(hir::ActorDef {
        name: name.to_string(),
        type_params: Vec::new(),
        persistent: true,
        state_fields,
        behaviors,
        init: Vec::new(),
        events: Vec::new(),
        apply_handlers: Vec::new(),
        version: 1,
        migrations: Vec::new(),
        is_workflow: true,
        is_organization: false,
        is_agent: false,
        virtual_: false,
        tools: Vec::new(),
        semantic_memory_dimensions: None,
        procedural_memory_namespace: None,
        fallback_config: String::new(),
        retry_config: String::new(),
        span,
    })
}

/// Lower an expression into a fresh body that yields the expression's value.
pub fn lower_body(expr: &Expr) -> hir::Body {
    let mut body = hir::Body::new();
    let op = lower_expr(expr, &mut body);
    if !body.is_terminated() {
        body.set_terminator(hir::Terminator::Yield(op));
    }
    body
}

/// Lower an expression into a sequence of statements in `body`, returning an
/// operand that represents the expression's value.
/// Lower a single let-binding's value and emit the matching HIR `Stmt::Let`.
/// For ordinary values this lowers the expression and stores the result via
/// `RValue::Use`.  Self-referencing lambdas are NOT handled here — the
/// caller must check for them and emit a `RecClosure` before calling this.
fn lower_let_value(name: &str, value: &Expr, span: Span, body: &mut hir::Body) {
    let vop = lower_expr(value, body);
    let ty = vop.ty();
    body.push(hir::Stmt::Let {
        name: name.to_string(),
        ty: ty.clone(),
        value: hir::RValue::Use(vop),
        span,
    });
}

/// Process a chain of `Let` nodes iteratively, lowering each value and
/// pushing the HIR `Stmt::Let`, then lowering the final non-Let body.
/// Keeps stack depth bounded regardless of how many sequential
/// let-statements the parser spliced into the AST.
fn lower_let_chain(first_body: &Expr, body: &mut hir::Body) -> hir::Operand {
    let mut cur: &Expr = first_body;
    loop {
        match cur {
            Expr::Let {
                name,
                value,
                body: inner,
                span,
                ..
            } => {
                // Self-referencing lambda check — same logic as the
                // top-level Let arm so that `let rec`-style chains work
                // inside iterative processing.
                if let Expr::Lambda {
                    params,
                    body: lam_body,
                    ..
                } = value.as_ref()
                {
                    if lambda_references(name, params, lam_body) {
                        let func_body = with_fresh_defer_stack(|| lower_body(lam_body));
                        body.push(hir::Stmt::Let {
                            name: name.clone(),
                            ty: Type::unit(),
                            value: hir::RValue::RecClosure {
                                name: name.clone(),
                                params: params
                                    .iter()
                                    .map(|p| (p.name.clone(), resolve_type(&p.ty)))
                                    .collect(),
                                body: Box::new(func_body),
                                ty: Type::unit(),
                            },
                            span: *span,
                        });
                        cur = inner;
                        continue;
                    }
                }
                lower_let_value(name, value, *span, body);
                cur = inner;
            }
            _ => return lower_expr(cur, body),
        }
    }
}

/// Emit all active deferred expressions from all scopes in LIFO order.
/// Snapshots the defer stack to avoid RefCell double-borrow when deferred
/// expressions themselves contain blocks/if/match (which re-enter the stack).
fn emit_all_defers(body: &mut hir::Body) {
    let snapshot = DEFER_SCOPES.with(|s| s.borrow().clone());
    for scope in snapshot.iter().rev() {
        for (expr, _error_only) in scope.iter().rev() {
            let _ = lower_expr(expr, body);
        }
    }
}

/// Emit deferred expressions from scopes above the nearest loop boundary.
fn emit_defers_for_break(body: &mut hir::Body) {
    let (snapshot, boundary) = DEFER_SCOPES.with(|s| {
        LOOP_MARKERS.with(|m| (s.borrow().clone(), m.borrow().last().copied().unwrap_or(0)))
    });
    for scope_idx in (boundary..snapshot.len()).rev() {
        if let Some(scope) = snapshot.get(scope_idx) {
            for (expr, _error_only) in scope.iter().rev() {
                let _ = lower_expr(expr, body);
            }
        }
    }
}

pub fn lower_expr(expr: &Expr, body: &mut hir::Body) -> hir::Operand {
    if body.is_terminated() {
        // Dead code after an explicit `return`/`break`: don't lower it.
        return hir::Operand::Unit;
    }
    match expr {
        Expr::FString(parts, span) => {
            if parts.is_empty() {
                hir::Operand::Literal(ast::Literal::String(String::new()), Type::string())
            } else {
                let mut result = lower_expr(&parts[0], body);
                for part in parts.iter().skip(1) {
                    let r = lower_expr(part, body);
                    let ty = Type::string();
                    let temp = fresh_temp_name();
                    body.push(hir::Stmt::Let {
                        name: temp.clone(),
                        ty: ty.clone(),
                        value: hir::RValue::Binary(ast::BinOp::Add, result, r, ty.clone()),
                        span: *span,
                    });
                    result = hir::Operand::Var(temp, ty);
                }
                result
            }
        }
        Expr::Literal(lit, _span) => {
            let ty = literal_type(lit);
            hir::Operand::Literal(lit.clone(), ty)
        }
        Expr::Var(name, _span) => {
            // Inline signal values at compile time; the reactive runtime will
            // wire mutable updates on the client/server side.
            if let Some(init) = SIGNAL_INITS.with(|c| c.borrow().get(name).cloned()) {
                return lower_expr(&init, body);
            }
            hir::Operand::Var(name.clone(), Type::unit())
        }
        Expr::SelfRef(_) => hir::Operand::Var("self".to_string(), Type::unit()),
        Expr::CapAnnotate { expr, .. } => lower_expr(expr, body),
        Expr::Lambda {
            params,
            body: lb,
            span,
            ..
        } => {
            let lambda_body = with_fresh_defer_stack(|| lower_body(lb));
            let ty = Type::unit();
            let captures = lambda_captures(params, lb);
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Closure {
                    params: params
                        .iter()
                        .map(|p| (p.name.clone(), resolve_type(&p.ty)))
                        .collect(),
                    body: Box::new(lambda_body),
                    captures,
                    ty: ty.clone(),
                },
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::App { func, args, span } => {
            // Intercept AI runtime builtins: Pipeline.new(), Supervisor.new(),
            // Debate.new(...), and their method chains. Also intercept .run()
            // on pipeline/supervisor/debate instances.
            if let Some((base, field)) = is_ai_builtin_call(func) {
                let ty = Type::unit();
                let temp = fresh_temp_name();
                let rv = lower_ai_builtin(base, field, args, body, *span);
                body.push(hir::Stmt::Let {
                    name: temp.clone(),
                    ty: ty.clone(),
                    value: rv,
                    span: *span,
                });
                return hir::Operand::Var(temp, ty);
            }

            // Heuristic: `.run()` on any variable → resolve via variable name.
            if let Some(rv) = try_lower_run_call(func, args, body, *span) {
                let ty = Type::unit();
                let temp = fresh_temp_name();
                body.push(hir::Stmt::Let {
                    name: temp.clone(),
                    ty: ty.clone(),
                    value: rv,
                    span: *span,
                });
                return hir::Operand::Var(temp, ty);
            }

            // Resolve `using` params from `given` bindings and dict args.
            let mut extra_given_args: Vec<ast::Expr> = Vec::new();
            let mut extra_dict_args: Vec<ast::Expr> = Vec::new();
            if let ast::Expr::Var(fn_name, _) = func.as_ref() {
                FN_USING_PARAMS.with(|c| {
                    if let Some(using_names) = c.borrow().get(fn_name) {
                        for uname in using_names {
                            GIVEN_BINDINGS.with(|g| {
                                if let Some(val) = g.borrow().get(uname) {
                                    extra_given_args.push(val.clone());
                                }
                            });
                        }
                    }
                });
                FN_DICT_PARAMS.with(|c| {
                    if let Some(constraints) = c.borrow().get(fn_name) {
                        for (_tp, tv, class_names) in constraints {
                            if let Some(ct) = infer_type_arg(args, tv) {
                                for cn in class_names {
                                    extra_dict_args.push(ast::Expr::App {
                                        func: Box::new(ast::Expr::Var(
                                            format!("_impl_{}_{}", cn, ct),
                                            Span::default(),
                                        )),
                                        args: vec![],
                                        span: Span::default(),
                                    });
                                }
                            }
                        }
                    }
                });
            }
            // Typeclass method resolution: FieldAccess on a concrete type
            // or a constrained type variable → route through the dictionary.
            if let Expr::FieldAccess {
                expr: receiver_expr,
                field: method_name,
                ..
            } = func.as_ref()
            {
                if let Some(dict) = try_resolve_typeclass_dict(receiver_expr, method_name) {
                    let receiver = lower_expr(receiver_expr, body);
                    let mut aops = vec![receiver];
                    for a in args {
                        aops.push(lower_expr(a, body));
                    }
                    let ty = Type::unit();
                    let dict_temp = fresh_temp_name();
                    match dict {
                        DictKind::Constant(dict_name) => {
                            body.push(hir::Stmt::Let {
                                name: dict_temp.clone(),
                                ty: ty.clone(),
                                value: hir::RValue::Call {
                                    func: hir::Operand::Var(dict_name, ty.clone()),
                                    args: vec![],
                                    ty: ty.clone(),
                                },
                                span: *span,
                            });
                        }
                        DictKind::Param(dict_name) => {
                            body.push(hir::Stmt::Let {
                                name: dict_temp.clone(),
                                ty: ty.clone(),
                                value: hir::RValue::Use(hir::Operand::Var(dict_name, ty.clone())),
                                span: *span,
                            });
                        }
                    }
                    let method_temp = fresh_temp_name();
                    body.push(hir::Stmt::Let {
                        name: method_temp.clone(),
                        ty: ty.clone(),
                        value: hir::RValue::FieldAccess {
                            base: hir::Operand::Var(dict_temp, ty.clone()),
                            field: method_name.clone(),
                            ty: ty.clone(),
                        },
                        span: *span,
                    });
                    let call_temp = fresh_temp_name();
                    body.push(hir::Stmt::Let {
                        name: call_temp.clone(),
                        ty: ty.clone(),
                        value: hir::RValue::Call {
                            func: hir::Operand::Var(method_temp, ty.clone()),
                            args: aops,
                            ty: ty.clone(),
                        },
                        span: *span,
                    });
                    return hir::Operand::Var(call_temp, ty);
                }
            }

            let fop = lower_expr(func, body);
            let mut aops: Vec<_> = args.iter().map(|a| lower_expr(a, body)).collect();
            for g in &extra_given_args {
                aops.push(lower_expr(g, body));
            }
            for d in &extra_dict_args {
                aops.push(lower_expr(d, body));
            }
            // Look up inferred return type for direct function calls
            let ty = CURRENT_INFERRED_DECL_TYPES.with(|cell| {
                cell.borrow()
                    .as_ref()
                    .and_then(|map| {
                        if let hir::Operand::Var(fn_name, _) = &fop {
                            map.get(fn_name).and_then(|t| match t {
                                Type::Function { ret, .. } => Some((**ret).clone()),
                                _ => None,
                            })
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(Type::unit)
            });
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Call {
                    func: fop,
                    args: aops,
                    ty: ty.clone(),
                },
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::Let {
            name,
            value,
            body: b,
            span,
            let_in,
            ..
        } => {
            // When the let has an explicit `in`-body (let_in == true),
            // the binding must be scoped to the body only — it must not
            // leak to subsequent expressions in the enclosing block.
            // We accomplish this by lowering the let-in into a fresh
            // HIR body and wrapping it in an RValue::Block, which the
            // MIR lower translates into a push/pop scope pair.
            if *let_in {
                // Let-bound lambdas may reference themselves within a
                // let-in body just as in a statement-let.
                if let Expr::Lambda {
                    params,
                    body: lam_body,
                    ..
                } = value.as_ref()
                {
                    if lambda_references(name, params, lam_body) {
                        let func_body = with_fresh_defer_stack(|| lower_body(lam_body));
                        let mut inner_body = hir::Body::new();
                        inner_body.push(hir::Stmt::Let {
                            name: name.clone(),
                            ty: Type::unit(),
                            value: hir::RValue::RecClosure {
                                name: name.clone(),
                                params: params
                                    .iter()
                                    .map(|p| (p.name.clone(), resolve_type(&p.ty)))
                                    .collect(),
                                body: Box::new(func_body),
                                ty: Type::unit(),
                            },
                            span: *span,
                        });
                        let op = lower_let_chain(b, &mut inner_body);
                        if !inner_body.is_terminated() {
                            inner_body.set_terminator(hir::Terminator::Yield(op));
                        }
                        let temp = fresh_temp_name();
                        body.push(hir::Stmt::Let {
                            name: temp.clone(),
                            ty: Type::unit(),
                            value: hir::RValue::Block(Box::new(inner_body)),
                            span: *span,
                        });
                        return hir::Operand::Var(temp, Type::unit());
                    }
                }
                // Standard let-in: lower into a scoped body.
                let mut inner_body = hir::Body::new();
                lower_let_value(name, value, *span, &mut inner_body);
                let op = lower_let_chain(b, &mut inner_body);
                if !inner_body.is_terminated() {
                    inner_body.set_terminator(hir::Terminator::Yield(op));
                }
                let temp = fresh_temp_name();
                body.push(hir::Stmt::Let {
                    name: temp.clone(),
                    ty: Type::unit(),
                    value: hir::RValue::Block(Box::new(inner_body)),
                    span: *span,
                });
                hir::Operand::Var(temp, Type::unit())
            } else {
                // Statement-let: let-bound lambdas may reference themselves.
                if let Expr::Lambda {
                    params,
                    body: lam_body,
                    ..
                } = value.as_ref()
                {
                    if lambda_references(name, params, lam_body) {
                        let func_body = with_fresh_defer_stack(|| lower_body(lam_body));
                        body.push(hir::Stmt::Let {
                            name: name.clone(),
                            ty: Type::unit(),
                            value: hir::RValue::RecClosure {
                                name: name.clone(),
                                params: params
                                    .iter()
                                    .map(|p| (p.name.clone(), resolve_type(&p.ty)))
                                    .collect(),
                                body: Box::new(func_body),
                                ty: Type::unit(),
                            },
                            span: *span,
                        });
                        return lower_let_chain(b, body);
                    }
                }
                // Standard let binding: lower the value, push the HIR stmt,
                // then process the body — and any chained lets — iteratively
                // to keep stack depth bounded regardless of how many sequential
                // let-statements the parser spliced into the AST.
                lower_let_value(name, value, *span, body);
                lower_let_chain(b, body)
            }
        }
        Expr::LetRec {
            name,
            params,
            value,
            body: b,
            span,
        } => {
            let func_body = with_fresh_defer_stack(|| lower_body(value));
            body.push(hir::Stmt::Let {
                name: name.clone(),
                ty: Type::unit(),
                value: hir::RValue::RecClosure {
                    name: name.clone(),
                    params: params
                        .iter()
                        .map(|p| (p.name.clone(), resolve_type(&p.ty)))
                        .collect(),
                    body: Box::new(func_body),
                    ty: Type::unit(),
                },
                span: *span,
            });
            lower_let_chain(b, body)
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            span,
        } => {
            let cond_op = lower_expr(cond, body);
            let ty = Type::unit();
            let temp = fresh_temp_name();
            let then_body = lower_body(then_branch);
            let else_body = else_branch.as_ref().map(|e| Box::new(lower_body(e)));
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::If {
                    cond: cond_op,
                    then_body: Box::new(then_body),
                    else_body,
                    ty: ty.clone(),
                },
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::Match {
            scrutinee,
            arms,
            span,
        } => {
            let scrut_op = lower_expr(scrutinee, body);
            let ty = Type::unit();
            let temp = fresh_temp_name();
            let arms_hir: Vec<_> = arms
                .iter()
                .map(|(pat, guard, e)| {
                    let guard_hir = guard.as_ref().map(|g| Box::new(lower_body(g)));
                    (pat.clone(), guard_hir, Box::new(lower_body(e)))
                })
                .collect();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Match {
                    scrutinee: scrut_op,
                    arms: arms_hir,
                    ty: ty.clone(),
                },
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::Block { exprs, span: _ } | Expr::Par { exprs, span: _ } => {
            push_defer_scope();
            let mut last = hir::Operand::Unit;
            for e in exprs {
                if body.is_terminated() {
                    break;
                }
                if let Expr::Defer {
                    expr, error_only, ..
                } = e
                {
                    add_defer((**expr).clone(), *error_only);
                    continue;
                }
                last = lower_expr(e, body);
            }
            if !body.is_terminated() {
                let scope = pop_defer_scope();
                for (expr, _error_only) in scope.into_iter().rev() {
                    if body.is_terminated() {
                        break;
                    }
                    let _ = lower_expr(&expr, body);
                }
            } else {
                let _ = pop_defer_scope();
            }
            last
        }
        Expr::Tuple(elems, span) => {
            let ops: Vec<_> = elems.iter().map(|e| lower_expr(e, body)).collect();
            let ty = Type::unit();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Tuple(ops, ty.clone()),
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::Record(fields, span) => {
            let fs: Vec<_> = fields
                .iter()
                .map(|(n, e)| (n.clone(), lower_expr(e, body)))
                .collect();
            let ty = Type::unit();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Record(fs, ty.clone()),
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::RecordUpdate { base, fields, span } => {
            let base_op = lower_expr(base, body);
            let overrides: Vec<_> = fields
                .iter()
                .map(|(n, e)| (n.clone(), lower_expr(e, body)))
                .collect();
            let ty = Type::unit();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::RecordUpdate {
                    base: base_op,
                    overrides,
                    ty: ty.clone(),
                },
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::FieldAccess { expr, field, span } => {
            let base = lower_expr(expr, body);
            let ty = Type::unit();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::FieldAccess {
                    base,
                    field: field.clone(),
                    ty: ty.clone(),
                },
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::Array(elems, span) => {
            let ops: Vec<_> = elems.iter().map(|e| lower_expr(e, body)).collect();
            let ty = Type::unit();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Array(ops, ty.clone()),
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::Index { arr, idx, span } => {
            let aop = lower_expr(arr, body);
            let iop = lower_expr(idx, body);
            let ty = Type::unit();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Index {
                    base: aop,
                    idx: iop,
                    ty: ty.clone(),
                },
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        // `self.field = v`, `arr[i] = v`, `record.f = v` are NOT parsed as
        // Expr::Assign (that node is only produced for a bare `ident = v`
        // prefix) — everywhere else, `=` is picked up by the Pratt parser's
        // infix loop as an ordinary-looking BinOp::Assign. Route it through
        // the same assignment lowering as Expr::Assign below.
        Expr::Binary {
            op: BinOp::Assign,
            left,
            right,
            span,
        } => lower_assign_to(left, right, *span, body),
        // Range: a .. b lowers to perform Array.range(a, b)
        Expr::Binary {
            op: BinOp::Range,
            left,
            right,
            span,
        } => {
            let l = lower_expr(left, body);
            let r = lower_expr(right, body);
            let ty = Type::Array(Box::new(Type::int()));
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Perform {
                    effect: "Array".to_string(),
                    op: "range".to_string(),
                    args: vec![l, r],
                    ty: ty.clone(),
                },
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::Binary {
            op,
            left,
            right,
            span,
        } => {
            let l = lower_expr(left, body);
            let r = lower_expr(right, body);
            let ty = binary_type(op, &l, &r);
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Binary(*op, l, r, ty.clone()),
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::Unary { op, expr, span } => {
            let e = lower_expr(expr, body);
            let ty = Type::unit();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Unary(*op, e, ty.clone()),
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::Assign {
            target,
            value,
            span,
        } => lower_assign_to(target, value, *span, body),
        Expr::Spawn {
            actor_type,
            init,
            target_node,
            span,
            ..
        } => {
            let name = actor_name_from_expr(actor_type).unwrap_or_default();
            let init_ops: Vec<_> = init
                .iter()
                .map(|(n, e)| (n.clone(), lower_expr(e, body)))
                .collect();
            let target_operand = target_node.as_ref().map(|e| lower_expr(e, body));
            let ty = Type::unit();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Spawn {
                    actor_type: name,
                    init: init_ops,
                    target_node: target_operand,
                    capabilities: vec![],
                    ty: ty.clone(),
                },
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::Send {
            actor,
            behavior,
            args,
            remote,
            span,
            ..
        } => {
            let aop = lower_expr(actor, body);
            let aops: Vec<_> = args.iter().map(|a| lower_expr(a, body)).collect();
            let ty = Type::unit();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Send {
                    actor: aop,
                    behavior: behavior.clone(),
                    args: aops,
                    remote: *remote,
                    ty: ty.clone(),
                },
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::Ask {
            actor,
            behavior,
            args,
            remote,
            timeout_ms,
            span,
        } => {
            let aop = lower_expr(actor, body);
            let aops: Vec<_> = args.iter().map(|a| lower_expr(a, body)).collect();
            let ty = Type::unit();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Ask {
                    actor: aop,
                    behavior: behavior.clone(),
                    args: aops,
                    remote: *remote,
                    timeout_ms: *timeout_ms,
                    ty: ty.clone(),
                },
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::GrainRef {
            grain_type,
            key,
            span,
        } => {
            // Syntactic sugar: Grain("Type", key) -> perform Grain.ref("Type", key).
            let perform = Expr::Perform {
                effect: "Grain".to_string(),
                op: "ref".to_string(),
                args: vec![
                    Expr::Literal(ast::Literal::String(grain_type.clone()), *span),
                    key.as_ref().clone(),
                ],
                span: *span,
            };
            lower_expr(&perform, body)
        }
        Expr::Perform {
            effect,
            op,
            args,
            span,
        } => {
            let aops: Vec<_> = args.iter().map(|a| lower_expr(a, body)).collect();
            let ty = Type::unit();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Perform {
                    effect: effect.clone(),
                    op: op.clone(),
                    args: aops,
                    ty: ty.clone(),
                },
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::Resume { value, span } => {
            let val_op = lower_expr(value, body);
            let ty = Type::unit();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Resume {
                    value: val_op,
                    ty: ty.clone(),
                },
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::Handle {
            body: hb,
            handlers,
            span,
        } => {
            let hbody = lower_body(hb);
            let hs: Vec<_> = handlers
                .iter()
                .map(|h| hir::EffectHandler {
                    effect_name: h.effect_name.clone(),
                    op_name: h.op_name.clone(),
                    params: h.params.iter().map(|p| (p.clone(), Type::unit())).collect(),
                    resume: h.resume,
                    body: Box::new(lower_body(&h.body)),
                    span: *span,
                })
                .collect();
            let ty = Type::unit();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Handle {
                    body: Box::new(hbody),
                    handlers: hs,
                    ty: ty.clone(),
                },
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::Receive { arms, after, span } => {
            let arms_hir: Vec<_> = arms
                .iter()
                .map(|(name, patterns, guard, e)| {
                    let guard_hir = guard.as_ref().map(|g| Box::new(lower_body(g)));
                    (
                        name.clone(),
                        patterns.clone(),
                        guard_hir,
                        Box::new(lower_body(e)),
                    )
                })
                .collect();
            let after_hir = after
                .as_ref()
                .map(|(ms, body)| (Box::new(lower_body(ms)), Box::new(lower_body(body))));
            let ty = Type::unit();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Receive {
                    arms: arms_hir,
                    after: after_hir,
                    ty: ty.clone(),
                },
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::Migrate { actor, node, span } => {
            let aop = lower_expr(actor, body);
            let nop = lower_expr(node, body);
            let ty = Type::unit();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value: hir::RValue::Migrate {
                    actor: aop,
                    node: nop,
                    ty: ty.clone(),
                },
                span: *span,
            });
            hir::Operand::Var(temp, ty)
        }
        Expr::Emit { event, args, span } => {
            let aops: Vec<_> = args.iter().map(|a| lower_expr(a, body)).collect();
            // Inject apply handler code BEFORE emit so the handler's
            // state mutation is visible to the runtime's +1 snapshot.
            CURRENT_APPLY_HANDLERS.with(|cell| {
                if let Some(ref handlers) = *cell.borrow() {
                    for handler in handlers {
                        if handler.event == *event {
                            let mut handler_body = hir::Body::new();
                            for (pi, param_name) in handler.params.iter().enumerate() {
                                let arg_op = if pi < aops.len() {
                                    aops[pi].clone()
                                } else {
                                    hir::Operand::Unit
                                };
                                handler_body.push(hir::Stmt::Let {
                                    name: param_name.clone(),
                                    ty: Type::unit(),
                                    value: hir::RValue::Use(arg_op),
                                    span: handler.span,
                                });
                            }
                            let result_op = lower_expr(&handler.body, &mut handler_body);
                            if !handler_body.is_terminated() {
                                handler_body.set_terminator(hir::Terminator::Yield(result_op));
                            }
                            for stmt in handler_body.stmts {
                                body.push(stmt);
                            }
                        }
                    }
                }
            });
            body.push(hir::Stmt::Emit {
                event: event.clone(),
                args: aops.clone(),
                span: *span,
            });
            hir::Operand::Unit
        }
        Expr::For {
            var,
            iterable,
            body: b,
            span,
        } => {
            let iop = lower_expr(iterable, body);
            push_loop_marker();
            let loop_body = lower_body(b);
            pop_loop_marker();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: Type::unit(),
                value: hir::RValue::For {
                    var: var.clone(),
                    iterable: iop,
                    body: Box::new(loop_body),
                },
                span: *span,
            });
            hir::Operand::Var(temp, Type::unit())
        }
        Expr::While {
            cond,
            body: b,
            span,
        } => {
            let cond_body = lower_body(cond);
            push_loop_marker();
            let loop_body = lower_body(b);
            pop_loop_marker();
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: Type::unit(),
                value: hir::RValue::While {
                    cond: Box::new(cond_body),
                    body: Box::new(loop_body),
                    span: *span,
                },
                span: *span,
            });
            hir::Operand::Var(temp, Type::unit())
        }
        Expr::Pipe { left, right, span } => {
            let app = match right.as_ref() {
                Expr::App {
                    func,
                    args,
                    span: app_span,
                } => {
                    let mut new_args = vec![left.as_ref().clone()];
                    new_args.extend(args.iter().cloned());
                    Expr::App {
                        func: func.clone(),
                        args: new_args,
                        span: *app_span,
                    }
                }
                _ => Expr::App {
                    func: right.clone(),
                    args: vec![left.as_ref().clone()],
                    span: *span,
                },
            };
            lower_expr(&app, body)
        }
        Expr::Return(val, _span) => {
            // Emit all active defers before return (all scopes, LIFO).
            emit_all_defers(body);
            let op = val.as_ref().map(|e| lower_expr(e, body));
            body.set_terminator(hir::Terminator::FnReturn(op));
            hir::Operand::Unit
        }
        Expr::Break(val, _span) => {
            emit_defers_for_break(body);
            let op = val.as_ref().map(|e| lower_expr(e, body));
            body.set_terminator(hir::Terminator::Break(op));
            hir::Operand::Unit
        }
        Expr::Consume { expr, .. } => lower_expr(expr, body),
        Expr::Recover { body: b, .. } => lower_expr(b, body),
        Expr::Defer { expr, .. } => {
            // Defer is handled at block level; standalone defer is a no-op.
            let _ = lower_expr(expr, body);
            hir::Operand::Unit
        }
        Expr::Hide { body: b, .. } | Expr::Seal { body: b, .. } => lower_expr(b, body),
        Expr::Panic(msg, span) => {
            let temp = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: temp.clone(),
                ty: Type::unit(),
                value: hir::RValue::Panic(msg.clone()),
                span: *span,
            });
            hir::Operand::Var(temp, Type::unit())
        }
        Expr::TypeAnnotate { expr, .. } => lower_expr(expr, body),
    }
}

/// Shared lowering for both `Expr::Assign` (bare `ident = v`) and
/// `Expr::Binary { op: BinOp::Assign, .. }` (`self.f = v`, `arr[i] = v`,
/// `record.f = v` — everything else, since only a bare identifier target is
/// special-cased by the parser's prefix position).
fn lower_assign_to(target: &Expr, value: &Expr, span: Span, body: &mut hir::Body) -> hir::Operand {
    let val = lower_expr(value, body);
    let place = lower_place(target, body);
    body.push(hir::Stmt::Assign {
        target: place,
        value: hir::RValue::Use(val.clone()),
        span,
    });
    val
}

fn lower_place(expr: &Expr, body: &mut hir::Body) -> hir::Place {
    match expr {
        Expr::Var(name, _) => hir::Place::Var(name.clone(), Type::unit()),
        // `self` always parses as SelfRef, never Var("self", _) — without this
        // arm, `self.field = value` would fall through to the generic
        // temp-materializing case below and lose the "this is self" marker
        // that lower_assign's place_is_self check depends on.
        Expr::SelfRef(_) => hir::Place::Var("self".to_string(), Type::unit()),
        Expr::FieldAccess {
            expr,
            field,
            span: _,
        } => {
            let base = lower_place(expr, body);
            hir::Place::Field {
                base: Box::new(base),
                field: field.clone(),
                ty: Type::unit(),
            }
        }
        Expr::Index { arr, idx, span: _ } => {
            let base = lower_place(arr, body);
            let idx_op = lower_expr(idx, body);
            hir::Place::Index {
                base: Box::new(base),
                idx: idx_op,
                ty: Type::unit(),
            }
        }
        _ => {
            let op = lower_expr(expr, body);
            let name = fresh_temp_name();
            body.push(hir::Stmt::Let {
                name: name.clone(),
                ty: op.ty(),
                value: hir::RValue::Use(op),
                span: Span::default(),
            });
            hir::Place::Var(name, Type::unit())
        }
    }
}

/// Free variables of a lambda (candidates for capture). The MIR lowering
/// filters this against what is actually in scope.
fn lambda_captures(params: &[crate::ast::Param], body: &Expr) -> Vec<String> {
    let bound: std::collections::HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
    let mut free = std::collections::HashSet::new();
    free_vars(body, &bound, &mut free);
    let mut captures: Vec<String> = free.into_iter().collect();
    captures.sort(); // deterministic ordering shared with codegen
    captures
}

/// Does a let-bound lambda reference its own binding name?
fn lambda_references(name: &str, params: &[crate::ast::Param], body: &Expr) -> bool {
    lambda_captures(params, body).iter().any(|c| c == name)
}

fn resolve_type(ty: &Option<Type>) -> Type {
    ty.clone().unwrap_or_else(Type::unit)
}

fn literal_type(lit: &Literal) -> Type {
    use crate::types::PrimitiveType;
    match lit {
        Literal::Int(_) => Type::Primitive(PrimitiveType::Int),
        Literal::Float(_) => Type::Primitive(PrimitiveType::Float),
        Literal::String(_) => Type::Primitive(PrimitiveType::String),
        Literal::Bool(_) => Type::Primitive(PrimitiveType::Bool),
        Literal::Nil => Type::Primitive(PrimitiveType::Nil),
        Literal::Unit => Type::Primitive(PrimitiveType::Unit),
    }
}

fn binary_type(op: &ast::BinOp, l: &hir::Operand, r: &hir::Operand) -> Type {
    use crate::ast::BinOp;
    use crate::types::PrimitiveType;
    match op {
        BinOp::Eq
        | BinOp::Ne
        | BinOp::Lt
        | BinOp::Le
        | BinOp::Gt
        | BinOp::Ge
        | BinOp::And
        | BinOp::Or => Type::Primitive(PrimitiveType::Bool),
        BinOp::Add => {
            // String concatenation: if either operand is a string, the
            // result is String. The typechecker guarantees both sides are
            // the same type, so checking one is sufficient — but we check
            // both to be robust against HIR-level type gaps (e.g. `perform`
            // expressions that are lowered with Type::unit() before their
            // real type is known).
            if l.ty() == Type::string() || r.ty() == Type::string() {
                Type::string()
            } else if is_float_operand(l) || is_float_operand(r) {
                Type::float()
            } else {
                Type::Primitive(PrimitiveType::Int)
            }
        }
        BinOp::Range => Type::Array(Box::new(Type::int())),
        // Arithmetic (Sub/Mul/Div/Mod/Pow) and any other non-boolean op:
        // propagate a Float result when either operand is a float, otherwise
        // Int. Without this, the result local's declared type is Int even for
        // float operands, so downstream `Unary Neg` (and friends) mis-compile
        // the float bits as an int (e.g. `-(0.1 + 0.22)`).
        _ => {
            if is_float_operand(l) || is_float_operand(r) {
                Type::float()
            } else {
                Type::Primitive(PrimitiveType::Int)
            }
        }
    }
}

/// Whether an HIR operand carries a statically-known float type. Variable
/// operands lower to `Type::unit()` (their precise type lives in the MIR
/// locals), so this is a best-effort check for literal/derived operands.
fn is_float_operand(o: &hir::Operand) -> bool {
    o.ty() == Type::float()
}
/// Returns `Some(builtin_name)` if the call's func is a field access on a
/// known builtin name (Pipeline, Supervisor, Debate), `None` otherwise.
fn is_ai_builtin_call(func: &Expr) -> Option<(&str, &str)> {
    match func {
        Expr::FieldAccess { expr, field, .. } => match expr.as_ref() {
            Expr::Var(name, _) => match name.as_str() {
                "Pipeline" | "Supervisor" | "Debate" => Some((name.as_str(), field.as_str())),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// Lower an AI runtime builtin call into an HIR RValue, lowering args
/// into the caller's body.
fn lower_ai_builtin(
    base_name: &str,
    field: &str,
    args: &[Expr],
    body: &mut hir::Body,
    _span: Span,
) -> hir::RValue {
    let ty = Type::unit();
    let mut a = |i: usize| {
        if i < args.len() {
            lower_expr(&args[i], body)
        } else {
            hir::Operand::Literal(Literal::String(String::new()), Type::string())
        }
    };

    match (base_name, field) {
        ("Pipeline", "new") => hir::RValue::PipelineNew { ty },
        ("Pipeline", "stage") => hir::RValue::PipelineStage {
            id: a(0),
            name: a(1),
            actor: a(2),
            template: a(3),
            ty,
        },
        ("Pipeline", "run") => hir::RValue::PipelineRun {
            id: a(0),
            input: a(1),
            ty,
        },
        ("Supervisor", "new") => hir::RValue::SupervisorNew { ty },
        ("Supervisor", "worker") => hir::RValue::SupervisorWorker {
            id: a(0),
            name: a(1),
            actor: a(2),
            description: a(3),
            ty,
        },
        ("Supervisor", "run") => hir::RValue::SupervisorRun {
            id: a(0),
            task: a(1),
            ty,
        },
        ("Debate", "new") => hir::RValue::DebateNew {
            topic: a(0),
            rounds: a(1),
            threshold: a(2),
            ty,
        },
        ("Debate", "participant") => hir::RValue::DebateParticipant {
            id: a(0),
            name: a(1),
            stance: a(2),
            actor: a(3),
            ty,
        },
        ("Debate", "run") => hir::RValue::DebateRun { id: a(0), ty },
        _ => unreachable!("is_ai_builtin_call should filter before lower_ai_builtin"),
    }
}

/// Heuristic: resolve `.run()` on a pipeline/supervisor/debate instance
/// by inspecting the variable name, mirroring the legacy compiler.
/// Lowers the receiver and args into `body`.
fn try_lower_run_call(
    func: &Expr,
    args: &[Expr],
    body: &mut hir::Body,
    _span: Span,
) -> Option<hir::RValue> {
    // Extract receiver and field from func: FieldAccess { expr: Var(name), field }
    let (base_name, receiver_expr, field) = match func {
        Expr::FieldAccess { expr, field, .. } => match expr.as_ref() {
            Expr::Var(name, _) => (name.as_str(), expr.as_ref(), field.as_str()),
            _ => return None,
        },
        _ => return None,
    };
    if field != "run" {
        return None;
    }

    let ty = Type::unit();
    let lowered = base_name.to_lowercase();

    // Lower the receiver (the pipeline/supervisor/debate variable) as the id.
    let id = lower_expr(receiver_expr, body);
    let mut a = |i: usize| {
        if i < args.len() {
            lower_expr(&args[i], body)
        } else {
            hir::Operand::Literal(Literal::String(String::new()), Type::string())
        }
    };

    if lowered.contains("debate") {
        Some(hir::RValue::DebateRun { id, ty })
    } else if lowered.contains("supervisor") || lowered.contains("team") {
        Some(hir::RValue::SupervisorRun { id, task: a(0), ty })
    } else {
        Some(hir::RValue::PipelineRun {
            id,
            input: a(0),
            ty,
        })
    }
}

fn actor_name_from_expr(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Var(name, _) => Some(name.clone()),
        _ => None,
    }
}

/// How a typeclass dictionary is resolved.
enum DictKind {
    Constant(String),
    Param(String),
}

fn try_resolve_typeclass_dict(receiver: &Expr, method_name: &str) -> Option<DictKind> {
    if let Expr::Var(name, _) = receiver {
        return CURRENT_FN_PARAMS.with(|cell| {
            let fn_params = cell.borrow();
            if let Some(Type::Var(tv)) = fn_params.get(name) {
                CURRENT_TYPE_PARAM_CONSTRAINTS.with(|cc| {
                    let constraints = cc.borrow();
                    for (tp_name, c_tv, class_names) in constraints.iter() {
                        if c_tv == tv {
                            for cn in class_names {
                                let found = CURRENT_CLASS_TABLES.with(|tc| {
                                    tc.borrow().as_ref().is_some_and(|tables| {
                                        tables.class_table.get(cn).is_some_and(|info| {
                                            info.methods.iter().any(|m| m.name == method_name)
                                        })
                                    })
                                });
                                if found {
                                    return Some(DictKind::Param(format!(
                                        "_dict_{}_{}",
                                        cn, tp_name
                                    )));
                                }
                            }
                        }
                    }
                    None
                })
            } else {
                None
            }
        });
    }
    // Literal receivers: concrete type is known, look up the impl constant.
    let type_name = match receiver {
        Expr::Literal(Literal::Int(_), _) => "Int",
        Expr::Literal(Literal::Float(_), _) => "Float",
        Expr::Literal(Literal::Bool(_), _) => "Bool",
        Expr::Literal(Literal::String(_), _) => "String",
        _ => return None,
    };
    CURRENT_CLASS_TABLES.with(|cell| {
        let tables = cell.borrow();
        let tables = tables.as_ref()?;
        for (class_name, class_info) in &tables.class_table {
            if class_info.methods.iter().any(|m| m.name == method_name) {
                let key = (class_name.clone(), type_name.to_string());
                if tables.instance_table.contains_key(&key) {
                    return Some(DictKind::Constant(format!(
                        "_impl_{}_{}",
                        class_name, type_name
                    )));
                }
            }
        }
        None
    })
}

fn infer_type_arg(args: &[ast::Expr], _tv: &TypeVar) -> Option<String> {
    for arg in args {
        match arg {
            ast::Expr::Literal(ast::Literal::Int(_), _) => return Some("Int".to_string()),
            ast::Expr::Literal(ast::Literal::Float(_), _) => return Some("Float".to_string()),
            ast::Expr::Literal(ast::Literal::Bool(_), _) => return Some("Bool".to_string()),
            ast::Expr::Literal(ast::Literal::String(_), _) => return Some("String".to_string()),
            _ => {}
        }
    }
    None
}

static TEMP_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn fresh_temp_name() -> String {
    let n = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("__tmp{}", n)
}

/// Thread-local apply handlers for the entity currently being lowered.
/// Set by `lower_behavior` before lowering each behavior body,
/// cleared afterwards. `lower_expr` checks this to inject apply handler
/// code after each `emit` site.
use std::cell::RefCell;
thread_local! {
    static CURRENT_APPLY_HANDLERS: RefCell<Option<Vec<ast::ApplyHandler>>> = RefCell::new(None);
}

// Thread-local class/instance tables for the module currently being lowered.
// Set by `lower_module` before lowering declarations, cleared afterwards.
// `lower_expr` checks this to resolve typeclass method calls through
// instance dictionaries.
thread_local! {
    static CURRENT_CLASS_TABLES: RefCell<Option<crate::typechecker::ClassTables>> = RefCell::new(None);
}

// Thread-local defer stack. Each Vec<(Expr, bool)> is one block scope.
// Shared across lower_body calls so that `return` inside if/match/loop
// branches drains the parent's defers too.
thread_local! {
    static DEFER_SCOPES: RefCell<Vec<Vec<(ast::Expr, bool)>>> = RefCell::new(Vec::new());
    /// Stack of scope indices marking loop boundaries. On `break`,
    /// defers are drained down to (but not including) the nearest loop marker.
    static LOOP_MARKERS: RefCell<Vec<usize>> = RefCell::new(Vec::new());
}

// Thread-local given bindings populated from `lower_module`.
thread_local! {
    static GIVEN_BINDINGS: RefCell<FxHashMap<String, ast::Expr>> = RefCell::new(FxHashMap::default());
    static FN_USING_PARAMS: RefCell<FxHashMap<String, Vec<String>>> = RefCell::new(FxHashMap::default());
    static FN_DICT_PARAMS: RefCell<FxHashMap<String, Vec<(String, TypeVar, Vec<String>)>>> = RefCell::new(FxHashMap::default());
    static SIGNAL_INITS: RefCell<FxHashMap<String, ast::Expr>> = RefCell::new(FxHashMap::default());
}

// Thread-local: current function's type parameter constraints and params.
thread_local! {
    #[allow(clippy::missing_const_for_thread_local)]
    static CURRENT_TYPE_PARAM_CONSTRAINTS: RefCell<Vec<(String, TypeVar, Vec<String>)>> = RefCell::new(Vec::new());
    #[allow(clippy::missing_const_for_thread_local)]
    static CURRENT_FN_PARAMS: RefCell<FxHashMap<String, Type>> = RefCell::new(FxHashMap::default());
}

// Thread-local: inferred function return types from type checker.
thread_local! {
    #[allow(clippy::missing_const_for_thread_local)]
    static CURRENT_INFERRED_DECL_TYPES: RefCell<Option<FxHashMap<String, Type>>> = RefCell::new(None);
}

/// Push a new defer scope (called when entering a block).
fn push_defer_scope() {
    DEFER_SCOPES.with(|s| s.borrow_mut().push(Vec::new()));
}

/// Pop the current defer scope and return its deferred expressions.
fn pop_defer_scope() -> Vec<(ast::Expr, bool)> {
    DEFER_SCOPES.with(|s| s.borrow_mut().pop().unwrap_or_default())
}

/// Add a defer to the current (innermost) scope.
fn add_defer(expr: ast::Expr, error_only: bool) {
    DEFER_SCOPES.with(|s| {
        if let Some(scope) = s.borrow_mut().last_mut() {
            scope.push((expr, error_only));
        }
    });
}

/// Mark the current scope depth as a loop boundary (called before lowering a loop body).
fn push_loop_marker() {
    DEFER_SCOPES.with(|s| LOOP_MARKERS.with(|m| m.borrow_mut().push(s.borrow().len())));
}

/// Remove the innermost loop marker (called after lowering a loop body).
fn pop_loop_marker() {
    LOOP_MARKERS.with(|m| {
        m.borrow_mut().pop();
    });
}

/// Save the current defer stack and loop markers, returning a snapshot.
fn save_defer_state() -> (Vec<Vec<(ast::Expr, bool)>>, Vec<usize>) {
    DEFER_SCOPES.with(|s| LOOP_MARKERS.with(|m| (s.borrow().clone(), m.borrow().clone())))
}

fn restore_defer_state(state: (Vec<Vec<(ast::Expr, bool)>>, Vec<usize>)) {
    DEFER_SCOPES.with(|s| {
        LOOP_MARKERS.with(|m| {
            *s.borrow_mut() = state.0;
            *m.borrow_mut() = state.1;
        })
    });
}

/// Run a closure with a fresh defer stack, restoring the previous state after.
fn with_fresh_defer_stack<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let saved = save_defer_state();
    DEFER_SCOPES.with(|s| s.borrow_mut().clear());
    LOOP_MARKERS.with(|m| m.borrow_mut().clear());
    let result = f();
    restore_defer_state(saved);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Param, Pattern};

    #[test]
    fn test_lower_literal() {
        let ast = ast::AstModule {
            name: "test".to_string(),
            decls: vec![Decl::Function {
                name: "__main".to_string(),
                type_params: vec![],
                type_param_constraints: vec![],
                params: vec![],
                default_values: vec![],
                using_params: vec![],
                ret_type: Some(Type::int()),
                error_type: None,
                effect: None,
                cap: None,
                requires: vec![],
                ensures: vec![],
                body: Expr::Literal(Literal::Int(42), Span::default()),
                annotations: vec![],
                public: true,
                span: Span::default(),
            }],
        };
        let hir = lower_module(&ast, &FxHashMap::default());
        assert_eq!(hir.decls.len(), 1);
    }

    #[test]
    fn test_lower_if_is_expression_positioned() {
        // `let x = if c then 1 else 2 in x` must keep the if as an RValue so
        // statements after it stay in evaluation order.
        let source_body = Expr::Let {
            name: "x".to_string(),
            ty: None,
            value: Box::new(Expr::If {
                cond: Box::new(Expr::Literal(Literal::Bool(true), Span::default())),
                then_branch: Box::new(Expr::Literal(Literal::Int(1), Span::default())),
                else_branch: Some(Box::new(Expr::Literal(Literal::Int(2), Span::default()))),
                span: Span::default(),
            }),
            body: Box::new(Expr::Var("x".to_string(), Span::default())),
            mutable: false,
            span: Span::default(),
            let_in: false,
        };
        let body = lower_body(&source_body);
        // The if lowers to a Let stmt with an RValue::If, then x's Let, and
        // the body yields x.
        assert!(matches!(body.terminator, hir::Terminator::Yield(_)));
        assert!(body.stmts.iter().any(|s| matches!(
            s,
            hir::Stmt::Let {
                value: hir::RValue::If { .. },
                ..
            }
        )));
    }

    /// Regression test: `self.field = value` must lower to an Assign whose
    /// target is `Place::Field { base: Place::Var("self", _), .. }`. Before
    /// the SelfRef arm was added to `lower_place`, the generic fallback
    /// materialized `self` into an unrelated temp, silently breaking the
    /// `place_is_self` check every self-assignment codegen path depends on.
    #[test]
    fn test_lower_self_field_assign_targets_self_place() {
        let expr = Expr::Assign {
            target: Box::new(Expr::FieldAccess {
                expr: Box::new(Expr::SelfRef(Span::default())),
                field: "count".to_string(),
                span: Span::default(),
            }),
            value: Box::new(Expr::Literal(Literal::Int(1), Span::default())),
            span: Span::default(),
        };
        let mut body = hir::Body::new();
        lower_expr(&expr, &mut body);
        let assign = body
            .stmts
            .iter()
            .find_map(|s| match s {
                hir::Stmt::Assign { target, .. } => Some(target),
                _ => None,
            })
            .expect("assignment statement should be present");
        match assign {
            hir::Place::Field { base, field, .. } => {
                assert_eq!(field, "count");
                assert!(
                    matches!(base.as_ref(), hir::Place::Var(name, _) if name == "self"),
                    "field base should be Place::Var(\"self\", _), got {:?}",
                    base
                );
            }
            other => panic!("expected Place::Field, got {:?}", other),
        }
    }

    /// Regression test: `free_vars` must descend into the effect/actor
    /// expression families (perform, handle, spawn, send, ask, receive,
    /// migrate, emit). Before that, variables used only inside those
    /// expressions were never captured by closures, and MIR lowering
    /// failed with "undefined variable".
    #[test]
    fn test_free_vars_covers_effect_and_actor_exprs() {
        use std::collections::HashSet;
        let span = Span::default();
        let var = |n: &str| Expr::Var(n.to_string(), span);
        let used = |expr: &Expr| {
            let mut acc = HashSet::new();
            free_vars(expr, &HashSet::new(), &mut acc);
            acc
        };

        // perform Effect.op(k)
        let perform = Expr::Perform {
            effect: "IO".to_string(),
            op: "print".to_string(),
            args: vec![var("k")],
            span,
        };
        assert!(used(&perform).contains("k"), "perform arg must be free");

        // emit Event(k)
        let emit = Expr::Emit {
            event: "E".to_string(),
            args: vec![var("k")],
            span,
        };
        assert!(used(&emit).contains("k"), "emit arg must be free");

        // a ! beh(k) and ask a beh(k): receiver and args are free
        let send = Expr::Send {
            actor: Box::new(var("a")),
            behavior: "beh".to_string(),
            args: vec![var("k")],
            remote: false,
            span,
        };
        let send_vars = used(&send);
        assert!(send_vars.contains("a") && send_vars.contains("k"));
        let ask = Expr::Ask {
            actor: Box::new(var("a")),
            behavior: "beh".to_string(),
            args: vec![var("k")],
            remote: false,
            timeout_ms: None,
            span,
        };
        let ask_vars = used(&ask);
        assert!(ask_vars.contains("a") && ask_vars.contains("k"));

        // spawn Actor { count = k }
        let spawn = Expr::Spawn {
            actor_type: Box::new(var("Counter")),
            init: vec![("count".to_string(), var("k"))],
            positional_args: None,
            register_as: None,
            target_node: None,
            span,
        };
        assert!(used(&spawn).contains("k"), "spawn init must be free");

        // migrate a to n
        let migrate = Expr::Migrate {
            actor: Box::new(var("a")),
            node: Box::new(var("n")),
            span,
        };
        let migrate_vars = used(&migrate);
        assert!(migrate_vars.contains("a") && migrate_vars.contains("n"));

        // handle k { | IO.print(m) => h }: body and handler-body vars are
        // free; the handler param is bound.
        let handle = Expr::Handle {
            body: Box::new(var("k")),
            handlers: vec![ast::EffectHandler {
                effect_name: "IO".to_string(),
                op_name: "print".to_string(),
                params: vec!["m".to_string()],
                body: var("h"),
                resume: true,
            }],
            span,
        };
        let handle_vars = used(&handle);
        assert!(handle_vars.contains("k"), "handle body var must be free");
        assert!(handle_vars.contains("h"), "handler body var must be free");
        assert!(!handle_vars.contains("m"), "handler param is bound");

        // receive { | Msg(p) => k + p }: arm var free, arm params bound.
        let receive = Expr::Receive {
            arms: vec![(
                "Msg".to_string(),
                vec![Pattern::Var("p".to_string())],
                None,
                Expr::Binary {
                    op: BinOp::Add,
                    left: Box::new(var("k")),
                    right: Box::new(var("p")),
                    span,
                },
            )],
            after: None,
            span,
        };
        let receive_vars = used(&receive);
        assert!(receive_vars.contains("k"), "receive arm var must be free");
        assert!(!receive_vars.contains("p"), "receive arm param is bound");

        // receive { | Msg() => 0 } after k => t: timeout expr and body are free.
        let receive_after = Expr::Receive {
            arms: vec![(
                "Msg".to_string(),
                vec![],
                None,
                Expr::Literal(Literal::Int(0), span),
            )],
            after: Some((Box::new(var("k")), Box::new(var("t")))),
            span,
        };
        let after_vars = used(&receive_after);
        assert!(
            after_vars.contains("k"),
            "receive-after ms expr must be free"
        );
        assert!(after_vars.contains("t"), "receive-after body must be free");
    }

    #[test]
    fn test_lower_state_machine_desugars_to_actor() {
        // A state_machine lowers to an ordinary hir::Decl::Actor (the desugar
        // targets the existing actor machinery — no new IR shapes).
        let sp = Span::default();
        let ast = ast::AstModule {
            name: "test".to_string(),
            decls: vec![Decl::StateMachine {
                name: "TcpConnection".to_string(),
                states: vec!["Closed".to_string(), "Connected".to_string()],
                events: vec![
                    ast::StateMachineEvent {
                        name: "connect".to_string(),
                        params: vec![Param::new("address", None)],
                        target: "Connected".to_string(),
                        span: sp,
                    },
                    ast::StateMachineEvent {
                        name: "disconnect".to_string(),
                        params: vec![],
                        target: "Closed".to_string(),
                        span: sp,
                    },
                ],
                entry_hooks: vec![("Connected".to_string(), Expr::Literal(Literal::Unit, sp))],
                exit_hooks: vec![],
                span: sp,
            }],
        };
        let hir = lower_module(&ast, &FxHashMap::default());
        assert_eq!(hir.decls.len(), 1);
        match &hir.decls[0] {
            hir::Decl::Actor(def) => {
                assert_eq!(def.name, "TcpConnection");
                assert!(!def.persistent);
                assert_eq!(def.state_fields.len(), 1);
                assert_eq!(def.state_fields[0].0, "_sm_state");
                assert_eq!(def.behaviors.len(), 2);
                assert_eq!(def.behaviors[0].name, "connect");
                assert_eq!(def.behaviors[0].params.len(), 1);
                assert_eq!(def.behaviors[1].name, "disconnect");
                // The transition lowers to a `_sm_state` field assign in the
                // event behavior body.
                assert!(
                    def.behaviors[0]
                        .body
                        .stmts
                        .iter()
                        .any(|s| matches!(s, hir::Stmt::Assign { .. })),
                    "event behavior should assign _sm_state"
                );
            }
            other => panic!("Expected actor declaration, got {:?}", other),
        }
    }
}

// ---------------------------------------------------------------------------
// Free variable analysis (moved from compiler.rs)
// ---------------------------------------------------------------------------

/// Collect all variable names bound by a pattern.
fn pattern_bindings(pat: &crate::ast::Pattern, out: &mut std::collections::HashSet<String>) {
    use crate::ast::Pattern;
    match pat {
        Pattern::Wild | Pattern::Lit(_) => {}
        Pattern::Var(name) | Pattern::Alias(name, _) => {
            out.insert(name.clone());
        }
        Pattern::Tuple(pats) => {
            for p in pats {
                pattern_bindings(p, out);
            }
        }
        Pattern::Record(fields) => {
            for (_, p) in fields {
                pattern_bindings(p, out);
            }
        }
        Pattern::Variant(_, Some(inner)) => pattern_bindings(inner, out),
        Pattern::Variant(_, None) => {}
    }
}

/// Collect free variables of an expression (variables used but not bound
/// within the expression). Shared between compiler and HIR lowering.
fn free_vars(
    expr: &crate::ast::Expr,
    bound: &std::collections::HashSet<String>,
    acc: &mut std::collections::HashSet<String>,
) {
    use crate::ast::Expr;
    match expr {
        Expr::Var(name, _) if !bound.contains(name) => {
            acc.insert(name.clone());
        }
        Expr::Var(_, _) => {}
        Expr::Lambda { params, body, .. } => {
            let mut new_bound = bound.clone();
            for p in params {
                new_bound.insert(p.name.clone());
            }
            free_vars(body, &new_bound, acc);
        }
        Expr::App { func, args, .. } => {
            free_vars(func, bound, acc);
            for a in args {
                free_vars(a, bound, acc);
            }
        }
        Expr::Let {
            name, value, body, ..
        } => {
            free_vars(value, bound, acc);
            let mut new_bound = bound.clone();
            new_bound.insert(name.clone());
            free_vars(body, &new_bound, acc);
        }
        Expr::LetRec {
            name,
            params,
            value,
            body,
            ..
        } => {
            let mut value_bound = bound.clone();
            value_bound.insert(name.clone());
            for p in params {
                value_bound.insert(p.name.clone());
            }
            free_vars(value, &value_bound, acc);
            let mut body_bound = bound.clone();
            body_bound.insert(name.clone());
            free_vars(body, &body_bound, acc);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            free_vars(cond, bound, acc);
            free_vars(then_branch, bound, acc);
            if let Some(e) = else_branch {
                free_vars(e, bound, acc);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            free_vars(scrutinee, bound, acc);
            for (pat, guard, arm_expr) in arms {
                let mut arm_bound = bound.clone();
                pattern_bindings(pat, &mut arm_bound);
                if let Some(guard_expr) = guard {
                    free_vars(guard_expr, &arm_bound, acc);
                }
                free_vars(arm_expr, &arm_bound, acc);
            }
        }
        Expr::Block { exprs, .. }
        | Expr::Par { exprs, .. }
        | Expr::Tuple(exprs, _)
        | Expr::Array(exprs, _) => {
            for e in exprs {
                free_vars(e, bound, acc);
            }
        }
        Expr::Record(fields, _) => {
            for (_, e) in fields {
                free_vars(e, bound, acc);
            }
        }
        Expr::RecordUpdate { base, fields, .. } => {
            free_vars(base, bound, acc);
            for (_, e) in fields {
                free_vars(e, bound, acc);
            }
        }
        Expr::FieldAccess { expr, .. } => free_vars(expr, bound, acc),
        Expr::Index { arr, idx, .. } => {
            free_vars(arr, bound, acc);
            free_vars(idx, bound, acc);
        }
        Expr::Binary { left, right, .. } => {
            free_vars(left, bound, acc);
            free_vars(right, bound, acc);
        }
        Expr::Unary { expr, .. } => free_vars(expr, bound, acc),
        Expr::Pipe { left, right, .. } => {
            free_vars(left, bound, acc);
            free_vars(right, bound, acc);
        }
        Expr::Assign { target, value, .. } => {
            free_vars(target, bound, acc);
            free_vars(value, bound, acc);
        }
        Expr::For {
            var,
            iterable,
            body,
            ..
        } => {
            free_vars(iterable, bound, acc);
            let mut new_bound = bound.clone();
            new_bound.insert(var.clone());
            free_vars(body, &new_bound, acc);
        }
        Expr::While { cond, body, .. } => {
            free_vars(cond, bound, acc);
            free_vars(body, bound, acc);
        }
        Expr::Return(Some(e), _) => {
            free_vars(e, bound, acc);
        }
        Expr::Return(None, _) => {}
        Expr::TypeAnnotate { expr, .. } | Expr::CapAnnotate { expr, .. } => {
            free_vars(expr, bound, acc)
        }
        Expr::Spawn {
            actor_type, init, ..
        } => {
            free_vars(actor_type, bound, acc);
            for (_, e) in init {
                free_vars(e, bound, acc);
            }
        }
        Expr::Send { actor, args, .. } | Expr::Ask { actor, args, .. } => {
            free_vars(actor, bound, acc);
            for a in args {
                free_vars(a, bound, acc);
            }
        }
        Expr::Emit { args, .. } | Expr::Perform { args, .. } => {
            for a in args {
                free_vars(a, bound, acc);
            }
        }
        Expr::Resume { value, .. } => {
            free_vars(value, bound, acc);
        }
        Expr::Handle { body, handlers, .. } => {
            free_vars(body, bound, acc);
            for h in handlers {
                let mut handler_bound = bound.clone();
                for p in &h.params {
                    handler_bound.insert(p.clone());
                }
                free_vars(&h.body, &handler_bound, acc);
            }
        }
        Expr::Receive { arms, after, .. } => {
            for (_, patterns, guard, arm_expr) in arms {
                let mut arm_bound = bound.clone();
                for pat in patterns {
                    pattern_bindings(pat, &mut arm_bound);
                }
                if let Some(g) = guard {
                    free_vars(g, &arm_bound, acc);
                }
                free_vars(arm_expr, &arm_bound, acc);
            }
            if let Some((ms, timeout_body)) = after {
                free_vars(ms, bound, acc);
                free_vars(timeout_body, bound, acc);
            }
        }
        Expr::Migrate { actor, node, .. } => {
            free_vars(actor, bound, acc);
            free_vars(node, bound, acc);
        }
        Expr::Consume { expr, .. } => {
            free_vars(expr, bound, acc);
        }
        Expr::Recover { body: b, .. } => {
            free_vars(b, bound, acc);
        }
        _ => {}
    }
}
