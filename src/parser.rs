//! Recursive descent parser for Nulang.
//!
//! Uses Pratt parser (precedence climbing) for expressions.
//! Entry point: `Parser::parse_module()`.

use crate::ast::*;
use crate::lexer::{Token, TokenKind};
use crate::types::{
    Capability, Effect, EffectRow, NuError, NuResult, NuWarning, PrimitiveType, Region, Span, Type,
    TypeVar,
};
use std::sync::OnceLock;
type FxHashMap<K, V> =
    std::collections::HashMap<K, V, std::hash::BuildHasherDefault<rustc_hash::FxHasher>>;

/// Resolved prelude variant types (`Option[T]`, `Result[Ok, Err]`), keyed
/// by name: `(type-parameter vars, expanded body)`. The prelude's type
/// declarations are prepended to the AST *after* the user module is
/// parsed, so the parser never saw them — yet prelude *constructors*
/// (`Ok(42)`, `Some(x)`) type-check in every module. That asymmetry meant
/// `let ok = Ok(42)` worked while `fn f(x: Option[Int])` failed to parse
/// ("Unknown type name"). Seeding every `Parser` with the prelude's
/// resolved decls (via the same `imported_type_cache` machinery imports
/// use) makes prelude types usable in annotations too. Local declarations
/// still shadow: `resolve_named_type` checks local decls first.
static PRELUDE_TYPE_CACHE: OnceLock<Vec<(String, Vec<TypeVar>, Type)>> = OnceLock::new();

// ---------------------------------------------------------------------------
// Operator Precedence (13 levels, higher = tighter binding)
// ---------------------------------------------------------------------------

const PREC_LOWEST: u8 = 0;
const PREC_ASSIGN: u8 = 1; // = += -=
const PREC_PIPE: u8 = 2; // |>
const PREC_RANGE: u8 = 3; // .. (inclusive-exclusive range, between pipe and or)
const PREC_OR: u8 = 4; // ||
const PREC_AND: u8 = 5; // &&
const PREC_EQ: u8 = 6; // == !=
const PREC_CMP: u8 = 7; // < <= > >=
const PREC_TERM: u8 = 8; // + -
const PREC_FACTOR: u8 = 9; // * / %
const PREC_SHIFT: u8 = 10; // << >>
const PREC_BITAND: u8 = 11; // &
const PREC_BITXOR: u8 = 12; // ^
const PREC_BITOR: u8 = 13; // |
const PREC_EXP: u8 = 14; // ** (power, right-associative, tighter than unary -)
const PREC_PREFIX: u8 = 11; // ! - & (prefix)

/// True when `name` is a primitive type name (`Int`, `Float`, ...). These
/// lex as `UpperIdent` (the typechecker resolves them by name). Routing
/// decisions must use [`type_decl_body_is_alias`] below, which exempts
/// `Nil` — the canonical empty variant of a sum type.
fn is_primitive_type_name(name: &str) -> bool {
    matches!(
        name,
        "Int" | "Float" | "Bool" | "String" | "Nil" | "Unit" | "Never" | "Address"
    )
}

/// True when `name` (the first token of a `type` declaration body) must be
/// routed to the ALIAS path rather than the variant path. `Nil` is
/// deliberately NOT alias-routed: it is the canonical empty variant of a
/// sum type (`type Stream[T] = Nil | Cons(...)`), so excluding it keeps
/// `Nil | ...` bodies on the variant path. The remaining primitives
/// (`Int`, `String`, `Unit`, ...) are degenerate as variant names and
/// resolve as types everywhere else, so their declarations are aliases.
fn type_decl_body_is_alias(first: &str) -> bool {
    is_primitive_type_name(first) && first != "Nil"
}

fn prefix_precedence(op: &TokenKind) -> Option<(u8, bool)> {
    match op {
        TokenKind::Minus | TokenKind::Not | TokenKind::Bang => Some((PREC_PREFIX, true)),
        TokenKind::Ampersand => Some((PREC_PREFIX, true)),
        TokenKind::Star => Some((PREC_PREFIX, true)),
        _ => None,
    }
}

fn infix_precedence(op: &TokenKind) -> Option<(u8, bool)> {
    let (prec, right_assoc) = match op {
        TokenKind::Assign | TokenKind::PlusAssign | TokenKind::MinusAssign => (PREC_ASSIGN, true),
        TokenKind::PipeOp => (PREC_PIPE, false),
        TokenKind::DotDot => (PREC_RANGE, false),
        TokenKind::Or => (PREC_OR, false),
        TokenKind::And => (PREC_AND, false),
        TokenKind::Eq | TokenKind::Ne => (PREC_EQ, false),
        TokenKind::Lt | TokenKind::Le | TokenKind::Gt | TokenKind::Ge => (PREC_CMP, false),
        TokenKind::Plus | TokenKind::Minus => (PREC_TERM, false),
        TokenKind::Star | TokenKind::Slash | TokenKind::Percent => (PREC_FACTOR, false),
        TokenKind::Shl | TokenKind::Shr => (PREC_SHIFT, false),
        TokenKind::Ampersand => (PREC_BITAND, false),
        TokenKind::Caret => (PREC_BITXOR, false),
        TokenKind::Star2 => (PREC_EXP, true), // right-associative power
        TokenKind::Pipe3 => (PREC_BITOR, false),
        // NOTE: single `|` is intentionally omitted. It is used as a match-arm
        // separator and function-type delimiter, so bitwise OR uses `|||`.
        _ => return None,
    };
    Some((prec, right_assoc))
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    local_type_params: FxHashMap<String, TypeVar>,
    global_type_constructors: FxHashMap<String, TypeVar>,
    /// Accumulated parse errors from error-recovery. Callers that want
    /// all errors (not just the first) call `consumed_diagnostics()`.
    diagnostics: Vec<NuError>,
    /// Non-fatal warnings collected during parsing (e.g. RFC 0015
    /// `catch`/`fail` deprecations). Callers surface these with
    /// `take_warnings()`.
    warnings: Vec<NuWarning>,
    /// Cache of types imported from other modules, populated lazily when
    /// a type name isn't found in the local token stream. Each entry holds
    /// the declaration's type-parameter variables followed by the resolved
    /// body (with the parameters still free), so use-site type arguments can
    /// be substituted in on cache hits.
    imported_type_cache: FxHashMap<String, (Vec<TypeVar>, Type)>,
    /// Module-level named handler registry.
    handler_registry: FxHashMap<String, Vec<EffectHandler>>,
}

/// Parsed app block; kept private to the parser and desugared into
/// ordinary functions before the AST leaves the parser.
struct ParsedApp {
    _name: String,
    routes: Vec<(String, String, String)>,
    span: Span,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        let mut parser = Self::new_raw(tokens);
        parser.seed_prelude_types();
        parser
    }

    /// Construct a parser without seeding prelude type names. Used by the
    /// prelude cache itself to avoid `OnceLock::get_or_init` reentrancy.
    fn new_raw(tokens: Vec<Token>) -> Self {
        Parser {
            tokens,
            pos: 0,
            global_type_constructors: FxHashMap::default(),
            local_type_params: FxHashMap::default(),
            imported_type_cache: FxHashMap::default(),
            handler_registry: FxHashMap::default(),
            diagnostics: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Make the prelude's variant types (`Option[T]`, `Result[Ok, Err]`)
    /// resolvable in type annotations. Resolves each prelude `type` decl
    /// once (lazily, via a raw parser so seeding is not re-entrant) and
    /// splices the `(param vars, expanded body)` pairs into this parser's
    /// imported-type cache — the same path `import stdlib::*` uses, so
    /// use-site type arguments are substituted on cache hits.
    fn seed_prelude_types(&mut self) {
        let entries = PRELUDE_TYPE_CACHE.get_or_init(|| {
            let source = crate::prelude_source::PRELUDE_SOURCE;
            let mut lexer = crate::lexer::Lexer::new(source);
            let tokens = match lexer.lex() {
                Ok(t) => t,
                Err(_) => return Vec::new(), // prelude must lex; degrade to no seeding
            };
            let mut pp = Self::new_raw(tokens);
            let ast = match pp.parse_module() {
                Ok(a) => a,
                Err(_) => return Vec::new(),
            };
            let mut out = Vec::new();
            for decl in &ast.decls {
                if let Decl::VariantType { name, span, .. } = decl {
                    if let Some(decl_pos) = pp.find_type_decl(name) {
                        if let Ok((param_vars, ty)) =
                            pp.resolve_local_type(name, &[], decl_pos, *span)
                        {
                            out.push((name.clone(), param_vars, ty));
                        }
                    }
                }
            }
            out
        });
        for (name, param_vars, ty) in entries {
            self.imported_type_cache
                .insert(name.clone(), (param_vars.clone(), ty.clone()));
        }
    }

    #[tracing::instrument(level = "debug", skip(self))]
    pub fn parse_module(&mut self) -> NuResult<AstModule> {
        self.diagnostics.clear();
        let mut decls = Vec::new();
        let mut pending_lets: Vec<Decl> = Vec::new();
        let mut app_decls: Vec<ParsedApp> = Vec::new();
        self.skip_newlines();
        while !self.is_at_end() {
            self.skip_newlines();
            if self.is_at_end() {
                break;
            }

            // App blocks are handled before other declarations.
            if self.match_token(&TokenKind::App) {
                app_decls.push(self.parse_app_decl()?);
                continue;
            }

            // Try declaration first, then expression
            let decl_start = self.pos;
            match self.parse_decl() {
                Ok(mut decl) => {
                    // Collect LetBinding decls — they'll be wrapped into main's body.
                    if matches!(decl, Decl::LetBinding { .. }) {
                        pending_lets.push(decl);
                    } else {
                        // If this is fn main() and we have pending lets, wrap them in.
                        if let Decl::Function { name, ref body, .. } = &decl {
                            if name == "main" && !pending_lets.is_empty() {
                                let mut wrapped_body = body.clone();
                                for let_decl in pending_lets.iter().rev() {
                                    if let Decl::LetBinding {
                                        name,
                                        type_ann,
                                        value,
                                        ..
                                    } = let_decl
                                    {
                                        wrapped_body = Expr::Let {
                                            name: name.clone(),
                                            ty: type_ann.clone(),
                                            value: Box::new(value.clone()),
                                            mutable: false,
                                            body: Box::new(wrapped_body),
                                            let_in: false,
                                            span: Span::default(),
                                        };
                                    }
                                }
                                pending_lets.clear();
                                // Push the modified main function
                                if let Decl::Function {
                                    name,
                                    type_params,
                                    type_param_constraints,
                                    params,
                                    default_values,
                                    using_params,
                                    ret_type,
                                    error_type,
                                    effect,
                                    cap,
                                    annotations,
                                    public,
                                    span,
                                    ..
                                } = decl
                                {
                                    decls.push(Decl::Function {
                                        name,
                                        type_params,
                                        type_param_constraints,
                                        params,
                                        default_values,
                                        using_params,
                                        ret_type,
                                        error_type,
                                        effect,
                                        cap,
                                        requires: vec![],
                                        ensures: vec![],
                                        body: wrapped_body,
                                        annotations,
                                        public,
                                        span,
                                    });
                                }
                                continue;
                            }
                        }
                        // Multi-clause function merging: if this is a fn decl,
                        // peek ahead for additional clauses with the same name.
                        // First clause provides signature + catch-all body.
                        // Additional clauses: `fn name(pattern) = body` where
                        // pattern is a literal, wildcard, or variant.
                        if let Decl::Function {
                            ref name,
                            ref type_params,
                            ref params,
                            ref mut body,
                            ..
                        } = decl
                        {
                            let canonical_params = params.clone();
                            let mut additional_arms: Vec<(Pattern, Option<Expr>, Expr)> =
                                Vec::new();
                            loop {
                                let saved_pos = self.pos;
                                self.skip_newlines();
                                if self.peek_kind() == &TokenKind::Fn {
                                    let fn_pos = self.pos;
                                    self.advance(); // consume 'fn'
                                    if let Ok(next_name) = self.expect_ident("function name") {
                                        if next_name == *name {
                                            // Parse this clause: `fn name(pat) = body`
                                            let (clause_type_params, _) =
                                                self.parse_type_params_with_constraints()?;
                                            if clause_type_params != *type_params {
                                                return Err(NuError::parse_error(
                                                    format!(
                                                        "function clause '{}' has different type parameters than first clause",
                                                        name
                                                    ),
                                                    self.current_span(),
                                                ));
                                            }
                                            self.expect(TokenKind::LParen)?;
                                            let clause_pats =
                                                self.parse_clause_patterns(canonical_params.len())?;
                                            self.expect(TokenKind::RParen)?;
                                            self.expect(TokenKind::Assign)?;
                                            let clause_body = self.parse_expr()?;
                                            let pat = if clause_pats.len() == 1 {
                                                clause_pats.into_iter().next().unwrap()
                                            } else {
                                                Pattern::Tuple(clause_pats)
                                            };
                                            additional_arms.push((pat, None, clause_body));
                                            continue;
                                        }
                                    }
                                    self.pos = fn_pos;
                                }
                                self.pos = saved_pos;
                                break;
                            }

                            if !additional_arms.is_empty() {
                                // Build catch-all arm from first clause's params and body
                                let catch_all_pat = if canonical_params.len() == 1 {
                                    Pattern::Var(canonical_params[0].name.clone())
                                } else {
                                    Pattern::Tuple(
                                        canonical_params
                                            .iter()
                                            .map(|p| Pattern::Var(p.name.clone()))
                                            .collect(),
                                    )
                                };
                                let mut all_arms = additional_arms;
                                all_arms.push((catch_all_pat, None, body.clone()));

                                // Build scrutinee
                                let scrutinee = if canonical_params.len() == 1 {
                                    Expr::Var(canonical_params[0].name.clone(), Span::default())
                                } else {
                                    Expr::Tuple(
                                        canonical_params
                                            .iter()
                                            .map(|p| Expr::Var(p.name.clone(), Span::default()))
                                            .collect(),
                                        Span::default(),
                                    )
                                };
                                *body = Expr::Match {
                                    scrutinee: Box::new(scrutinee),
                                    arms: all_arms,
                                    span: Span::default(),
                                };
                            }
                        }
                        decls.push(decl);
                    }
                }
                Err(e) => {
                    let consumed = self.pos - decl_start;
                    if consumed > 0 {
                        return Err(e);
                    }
                    // Not a declaration — this must be the top-level script body.
                    let exprs = self.collect_block_exprs(None)?;
                    let final_expr = if exprs.len() == 1 {
                        exprs.into_iter().next().unwrap()
                    } else {
                        Expr::Block {
                            exprs,
                            span: Span::default(),
                        }
                    };
                    // If we have pending lets, wrap them around the final expression.
                    let mut body = final_expr;
                    for let_decl in pending_lets.iter().rev() {
                        if let Decl::LetBinding {
                            name,
                            type_ann,
                            value,
                            ..
                        } = let_decl
                        {
                            body = Expr::Let {
                                name: name.clone(),
                                ty: type_ann.clone(),
                                value: Box::new(value.clone()),
                                mutable: false,
                                body: Box::new(body),
                                let_in: false,
                                span: Span::default(),
                            };
                        }
                    }
                    pending_lets.clear();
                    decls.push(Decl::Function {
                        name: "__main".to_string(),
                        type_params: vec![],
                        type_param_constraints: vec![],
                        params: vec![],
                        default_values: vec![],
                        using_params: vec![],
                        ret_type: None,
                        error_type: None,
                        effect: None,
                        cap: None,
                        requires: vec![],
                        ensures: vec![],
                        body,
                        annotations: vec![],
                        public: false,
                        span: Span::new(0, 0),
                    });
                    break;
                }
            }
            self.skip_newlines_semicolons();
        }
        // If there are pending lets but no main function, create a synthetic __main.
        if !pending_lets.is_empty() {
            let mut body = Expr::Literal(Literal::Unit, Span::default());
            for let_decl in pending_lets.iter().rev() {
                if let Decl::LetBinding {
                    name,
                    type_ann,
                    value,
                    ..
                } = let_decl
                {
                    body = Expr::Let {
                        name: name.clone(),
                        ty: type_ann.clone(),
                        value: Box::new(value.clone()),
                        mutable: false,
                        body: Box::new(body),
                        let_in: false,
                        span: Span::default(),
                    };
                }
            }
            decls.push(Decl::Function {
                name: "__main".to_string(),
                type_params: vec![],
                type_param_constraints: vec![],
                params: vec![],
                default_values: vec![],
                using_params: vec![],
                ret_type: None,
                error_type: None,
                effect: None,
                cap: None,
                requires: vec![],
                ensures: vec![],
                body,
                annotations: vec![],
                public: false,
                span: Span::new(0, 0),
            });
        }
        if !self.diagnostics.is_empty() {
            return Err(NuError::Multiple(std::mem::take(&mut self.diagnostics)));
        }
        self.check_route_params(&app_decls, &decls)?;
        decls.extend(self.desugar_app_decls(app_decls));
        let decls = Self::expand_contracts(Self::expand_derives(decls));
        Ok(AstModule {
            name: "main".to_string(),
            decls,
        })
    }

    /// Desugar `@derive(eq)` on record types into a synthetic structural-equality
    /// function `{name}_eq(a, b) -> Bool`. Walks top-level and nested-module decls
    /// so a derive inside a `module { }` block is expanded too.
    fn expand_derives(decls: Vec<Decl>) -> Vec<Decl> {
        let mut out = Vec::with_capacity(decls.len());
        for decl in decls {
            match decl {
                Decl::RecordType {
                    name,
                    type_params,
                    fields,
                    derives,
                    public,
                    span,
                } => {
                    let wants_eq = derives.iter().any(|d| d == "eq");
                    out.push(Decl::RecordType {
                        name: name.clone(),
                        type_params: type_params.clone(),
                        fields: fields.clone(),
                        derives,
                        public,
                        span,
                    });
                    if wants_eq {
                        out.push(Self::derive_eq_function(&name, &fields, span));
                    }
                }
                Decl::Module {
                    name,
                    exports,
                    decls: inner,
                    span,
                } => {
                    out.push(Decl::Module {
                        name,
                        exports,
                        decls: Self::expand_derives(inner),
                        span,
                    });
                }
                other => out.push(other),
            }
        }
        out
    }

    /// Synthesize `fn {record_name}_eq(a, b) -> Bool` comparing every field with
    /// `==`, `&&`-chained. The zero-field case is trivially `true`.
    fn derive_eq_function(record_name: &str, fields: &[(String, Type)], span: Span) -> Decl {
        let rec_ty = Type::Record(fields.to_vec());
        let mut body: Option<Expr> = None;
        for (field, _) in fields {
            let cmp = Expr::Binary {
                op: BinOp::Eq,
                left: Box::new(Expr::FieldAccess {
                    expr: Box::new(Expr::Var("a".to_string(), span)),
                    field: field.clone(),
                    span,
                }),
                right: Box::new(Expr::FieldAccess {
                    expr: Box::new(Expr::Var("b".to_string(), span)),
                    field: field.clone(),
                    span,
                }),
                span,
            };
            body = Some(match body {
                None => cmp,
                Some(acc) => Expr::Binary {
                    op: BinOp::And,
                    left: Box::new(acc),
                    right: Box::new(cmp),
                    span,
                },
            });
        }
        let body = body.unwrap_or_else(|| Expr::Literal(Literal::Bool(true), span));
        Decl::Function {
            name: format!("{}_eq", record_name.to_lowercase()),
            type_params: vec![],
            type_param_constraints: vec![],
            params: vec![
                Param::new("a", Some(rec_ty.clone())),
                Param::new("b", Some(rec_ty)),
            ],
            default_values: vec![None, None],
            using_params: vec![],
            ret_type: Some(Type::bool()),
            error_type: None,
            effect: None,
            cap: None,
            requires: vec![],
            ensures: vec![],
            body,
            annotations: vec![],
            public: false,
            span,
        }
    }

    /// Desugar `requires` / `ensures` contract clauses into runtime checks:
    /// `requires` become entry guards, `ensures` become a `let result = <body>`
    /// wrapper that checks each postcondition against `result`. Violations raise a
    /// runtime panic (`OpCode::Panic`) with a stable category message.
    fn expand_contracts(decls: Vec<Decl>) -> Vec<Decl> {
        decls
            .into_iter()
            .map(|decl| match decl {
                Decl::Function {
                    name,
                    type_params,
                    type_param_constraints,
                    params,
                    default_values,
                    using_params,
                    ret_type,
                    error_type,
                    effect,
                    cap,
                    requires,
                    ensures,
                    body,
                    annotations,
                    public,
                    span,
                } => Decl::Function {
                    name,
                    type_params,
                    type_param_constraints,
                    params,
                    default_values,
                    using_params,
                    ret_type,
                    error_type,
                    effect,
                    cap,
                    requires: requires.clone(),
                    ensures: ensures.clone(),
                    body: Self::wrap_contracts(body, &requires, &ensures, span),
                    annotations,
                    public,
                    span,
                },
                Decl::Module {
                    name,
                    exports,
                    decls: inner,
                    span,
                } => Decl::Module {
                    name,
                    exports,
                    decls: Self::expand_contracts(inner),
                    span,
                },
                other => other,
            })
            .collect()
    }

    /// Wrap a function body with its contract checks.
    fn wrap_contracts(body: Expr, requires: &[Expr], ensures: &[Expr], span: Span) -> Expr {
        // Postconditions: `let result = body in if (e1 && e2) then result else panic`.
        let mut checked = body;
        if !ensures.is_empty() {
            let cond = Self::and_chain(ensures, span);
            checked = Expr::Let {
                name: "result".to_string(),
                ty: None,
                value: Box::new(checked),
                body: Box::new(Expr::If {
                    cond: Box::new(cond),
                    then_branch: Box::new(Expr::Var("result".to_string(), span)),
                    else_branch: Some(Box::new(Expr::Panic(
                        "postcondition_violation".to_string(),
                        span,
                    ))),
                    span,
                }),
                mutable: false,
                let_in: false,
                span,
            };
        }
        // Preconditions: `if (r1 && r2) then checked else panic`.
        if !requires.is_empty() {
            let cond = Self::and_chain(requires, span);
            checked = Expr::If {
                cond: Box::new(cond),
                then_branch: Box::new(checked),
                else_branch: Some(Box::new(Expr::Panic(
                    "precondition_violation".to_string(),
                    span,
                ))),
                span,
            };
        }
        checked
    }

    /// `&&`-chain a non-empty slice of boolean predicates.
    fn and_chain(exprs: &[Expr], span: Span) -> Expr {
        let mut it = exprs.iter().cloned();
        let first = it.next().expect("and_chain requires a non-empty slice");
        it.fold(first, |acc, e| Expr::Binary {
            op: BinOp::And,
            left: Box::new(acc),
            right: Box::new(e),
            span,
        })
    }

    /// Consume and return all diagnostics accumulated during error-recovery
    /// parsing. After this call, the diagnostics buffer is empty.
    pub fn consumed_diagnostics(&mut self) -> Vec<NuError> {
        std::mem::take(&mut self.diagnostics)
    }

    /// Consume and return all non-fatal warnings accumulated during parsing
    /// (e.g. RFC 0015 `catch`/`fail` deprecations). After this call, the
    /// warnings buffer is empty.
    pub fn take_warnings(&mut self) -> Vec<NuWarning> {
        std::mem::take(&mut self.warnings)
    }

    // === Declarations ===

    fn parse_decl(&mut self) -> NuResult<Decl> {
        self.local_type_params.clear();
        let _span = self.current_span();
        let annotations = self.parse_function_annotations()?;
        self.skip_newlines();
        let public = self.consume_if(&TokenKind::Pub);
        self.skip_newlines();
        match self.peek_kind() {
            TokenKind::Fn => self.parse_function(public, annotations),
            TokenKind::Actor
            | TokenKind::Persistent
            | TokenKind::Entity
            | TokenKind::Organization
            | TokenKind::Virtual => {
                let backend = annotations.iter().find_map(|a| match a {
                    crate::ast::FunctionAnnotation::Backend { kind } => Some(*kind),
                    _ => None,
                });
                self.parse_actor(backend)
            }
            TokenKind::StateMachine => self.parse_state_machine(),
            TokenKind::Agent => self.parse_agent(),
            TokenKind::Crdt => self.parse_crdt_decl(),
            TokenKind::Workflow => self.parse_workflow(),
            TokenKind::Database => self.parse_database(),
            TokenKind::Opaque => {
                self.advance(); // consume 'opaque'
                self.expect(TokenKind::Type)?; // 'type'
                self.skip_newlines();
                self.parse_type_alias(public, true)
            }
            TokenKind::Type => {
                self.advance(); // consume 'type'
                self.skip_newlines();
                let derives: Vec<String> = annotations
                    .iter()
                    .filter_map(|a| match a {
                        crate::ast::FunctionAnnotation::Derive(names) => Some(names.clone()),
                        _ => None,
                    })
                    .flatten()
                    .collect();
                match self.peek_kind() {
                    TokenKind::Alias => self.parse_type_alias(public, false),
                    _ => self.parse_type_decl_variant_or_record(public, derives),
                }
            }
            TokenKind::Handler => self.parse_named_handler(),
            TokenKind::Effect => self.parse_effect_decl(),
            TokenKind::Extern => self.parse_extern(public),
            TokenKind::Import => self.parse_import(),
            TokenKind::Module => {
                self.advance(); // consume 'module'
                let name = self.expect_ident("module name")?;
                self.expect(TokenKind::LBrace)?;
                let mut decls = Vec::new();
                self.skip_newlines();
                while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
                    self.skip_newlines();
                    if self.match_token(&TokenKind::RBrace) {
                        break;
                    }
                    decls.push(self.parse_decl()?);
                    self.skip_newlines();
                }
                self.expect(TokenKind::RBrace)?;
                Ok(Decl::Module {
                    name,
                    exports: vec![],
                    decls,
                    span: self.current_span(),
                })
            }
            TokenKind::Class => self.parse_class(),
            TokenKind::Let => self.parse_module_let(public),
            TokenKind::Impl => self.parse_impl(),
            TokenKind::Given => self.parse_given(public),
            TokenKind::Signal => self.parse_signal(),
            TokenKind::Eof => Err(NuError::parse_error(
                "Unexpected end of file in declaration".to_string(),
                self.current_span(),
            )),
            other => Err(NuError::parse_error(
                format!("Unexpected token in declaration: {}", other),
                self.current_span(),
            )),
        }
    }

    fn parse_function_annotations(&mut self) -> NuResult<Vec<FunctionAnnotation>> {
        let mut annotations = Vec::new();
        while self.consume_if(&TokenKind::At) {
            let name = match self.peek_kind() {
                TokenKind::Tool => {
                    self.advance();
                    "tool".to_string()
                }
                TokenKind::Ident(s) => {
                    let s = s.clone();
                    self.advance();
                    s
                }
                other => {
                    return Err(NuError::parse_error(
                        format!("Expected annotation name, found {}", other),
                        self.current_span(),
                    ));
                }
            };
            self.expect(TokenKind::LParen)?;
            let mut fields: FxHashMap<String, String> = FxHashMap::default();
            self.skip_newlines();
            while !self.match_token(&TokenKind::RParen) && !self.is_at_end() {
                let field_name = self.expect_ident("annotation field name")?;
                if self.consume_if(&TokenKind::Colon) {
                    let field_value = self.expect_string("annotation field value")?;
                    fields.insert(field_name, field_value);
                } else {
                    fields.insert(String::new(), field_name);
                }
                self.skip_newlines();
                if !self.consume_if(&TokenKind::Comma) {
                    break;
                }
                self.skip_newlines();
            }
            self.expect(TokenKind::RParen)?;
            match name.as_str() {
                "tool" => {
                    let description = fields.remove("description").unwrap_or_default();
                    annotations.push(FunctionAnnotation::Tool { description });
                }
                "backend" => {
                    let kind_str = fields
                        .remove("kind")
                        .or_else(|| fields.remove(""))
                        .unwrap_or_default();
                    let kind = match kind_str.as_str() {
                        "native" => crate::ast::ActorBackendKind::Native,
                        "wasm" => crate::ast::ActorBackendKind::WasmComponent,
                        other => {
                            return Err(NuError::parse_error(
                                format!("Unknown backend '{}'; expected 'native' or 'wasm'", other),
                                self.current_span(),
                            ))
                        }
                    };
                    annotations.push(FunctionAnnotation::Backend { kind });
                }
                "derive" => {
                    // `@derive(eq)` — nameless fields are stored under the
                    // empty key (no `:`), named ones under their field name.
                    let names: Vec<String> = fields
                        .into_iter()
                        .map(|(k, v)| if k.is_empty() { v } else { k })
                        .collect();
                    annotations.push(FunctionAnnotation::Derive(names));
                }
                "placement" => {
                    let value = fields.remove("").unwrap_or_default();
                    let placement = match value.as_str() {
                        "static" => crate::types::Placement::Static,
                        "server" => crate::types::Placement::Server,
                        "edge" => crate::types::Placement::Edge,
                        "client" => crate::types::Placement::Client,
                        "actor" => crate::types::Placement::Actor,
                        "workflow" => crate::types::Placement::Workflow,
                        other => {
                            return Err(NuError::parse_error(
                                format!(
                                    "Unknown placement '{}'; expected 'static', 'server', 'edge', 'client', 'actor', or 'workflow'",
                                    other
                                ),
                                self.current_span(),
                            ))
                        }
                    };
                    annotations.push(FunctionAnnotation::Placement(placement));
                }
                _ => {
                    return Err(NuError::parse_error(
                        format!("Unknown function annotation: @{}", name),
                        self.current_span(),
                    ));
                }
            }
        }
        Ok(annotations)
    }

    fn parse_function(
        &mut self,
        public: bool,
        annotations: Vec<FunctionAnnotation>,
    ) -> NuResult<Decl> {
        let span = self.current_span();
        self.advance(); // consume 'fn'
        let name = self.expect_ident("function name")?;

        // Type parameters [T, U] or [T: Ord]
        let (type_params, type_param_constraints) = self.parse_type_params_with_constraints()?;

        self.expect(TokenKind::LParen)?;
        let (params, default_values) = self.parse_params_with_defaults()?;
        self.expect(TokenKind::RParen)?;

        // Optional `using` clause: `fn foo(x) using (log: Logger) -> T`
        let using_params = if self.consume_if(&TokenKind::Using) {
            self.expect(TokenKind::LParen)?;
            let (up, _) = self.parse_params_with_defaults()?;
            self.expect(TokenKind::RParen)?;
            up
        } else {
            vec![]
        };
        let ret_type = if self.consume_if(&TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        // Error type and/or effect annotation.
        // `! { ... }` → effect row.  `! Type` → error type.
        let error_type;
        let effect;
        if self.consume_if(&TokenKind::Bang) {
            if self.peek_kind() == &TokenKind::LBrace {
                error_type = None;
                effect = Some(self.parse_effect_row()?);
            } else {
                error_type = Some(self.parse_type()?);
                effect = if self.consume_if(&TokenKind::Bang) || self.consume_if(&TokenKind::Throws)
                {
                    Some(self.parse_effect_row()?)
                } else {
                    None
                };
            }
        } else if self.consume_if(&TokenKind::Throws) {
            if self.peek_kind() == &TokenKind::LBrace {
                error_type = None;
                effect = Some(self.parse_effect_row()?);
            } else {
                error_type = Some(self.parse_type()?);
                effect = if self.consume_if(&TokenKind::Bang) || self.consume_if(&TokenKind::Throws)
                {
                    Some(self.parse_effect_row()?)
                } else {
                    None
                };
            }
        } else {
            error_type = None;
            effect = None;
        }
        let cap = if self.consume_if(&TokenKind::Colon) {
            Some(self.parse_capability()?)
        } else {
            None
        };

        // Optional `requires` / `ensures` contract clauses before the body.
        let mut requires = Vec::new();
        let mut ensures = Vec::new();
        loop {
            self.skip_newlines();
            if self.peek_kind() == &TokenKind::Requires {
                self.advance();
                requires.push(self.parse_expr()?);
            } else if self.peek_kind() == &TokenKind::Ensures {
                self.advance();
                ensures.push(self.parse_expr()?);
            } else {
                break;
            }
        }

        // Optional `=` for single-expression shorthand: `fn f() -> T = expr`
        let _ = self.consume_if(&TokenKind::Assign);
        let body = self.parse_expr()?;
        Ok(Decl::Function {
            name,
            type_params,
            type_param_constraints,
            params,
            default_values,
            using_params,
            ret_type,
            error_type,
            effect,
            cap,
            requires,
            ensures,
            body,
            annotations,
            public,
            span,
        })
    }

    fn parse_actor(&mut self, backend: Option<crate::ast::ActorBackendKind>) -> NuResult<Decl> {
        let span = self.current_span();
        let virtual_ = self.consume_if(&TokenKind::Virtual);
        let persistent = self.consume_if(&TokenKind::Persistent);
        let is_entity = self.consume_if(&TokenKind::Entity);
        let is_org = self.consume_if(&TokenKind::Organization);
        let persistent = persistent || is_entity || is_org;
        if virtual_ && !is_entity {
            return Err(NuError::parse_error(
                "'virtual' can only modify 'entity' declarations".to_string(),
                self.current_span(),
            ));
        }
        if !is_entity && !is_org {
            self.expect(TokenKind::Actor)?;
        }
        let default_model = if is_entity || is_org {
            StateModel::EventSourced
        } else {
            StateModel::Local
        };
        // For `persistent actor` without explicit model, keep the existing Local default
        // so existing behavior is unchanged; `entity` is the durable-first form.
        let name = self.expect_ident("actor name")?;
        let type_params = self.parse_type_params()?;
        let key_params = if virtual_ {
            self.expect(TokenKind::LParen)?;
            let params = self.parse_params()?;
            self.expect(TokenKind::RParen)?;
            params
        } else {
            vec![]
        };
        let implements = match self.peek_kind() {
            TokenKind::Ident(s) if s == "implements" => {
                self.advance();
                Some(self.expect_ident("contract name")?)
            }
            _ => None,
        };
        self.expect(TokenKind::LBrace)?;

        let mut state_fields = Vec::new();
        let mut behaviors = Vec::new();
        let mut initializer: Option<(String, Vec<crate::ast::Param>, Expr)> = None;
        let mut version: u32 = 1;
        let mut events: Vec<crate::ast::EventDecl> = Vec::new();
        let mut apply_handlers: Vec<crate::ast::ApplyHandler> = Vec::new();
        let mut migrations: Vec<crate::ast::MigrationDecl> = Vec::new();
        while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
            self.skip_newlines();
            if self.match_token(&TokenKind::RBrace) {
                break;
            }
            match self.peek_kind() {
                TokenKind::State => {
                    self.advance(); // 'state'
                    let model = self.parse_state_model(default_model);
                    let field_name = self.expect_ident("state field name")?;
                    let ty = if self.consume_if(&TokenKind::Colon) {
                        self.parse_type()?
                    } else {
                        Type::unit()
                    };
                    if !self.consume_if(&TokenKind::Assign) {
                        self.expect(TokenKind::Colon)?;
                    }
                    let default = self.parse_expr()?;
                    state_fields.push((field_name, model, ty, default));
                    self.skip_newlines_semicolons();
                }
                TokenKind::Behavior => {
                    behaviors.push(self.parse_behavior()?);
                }
                TokenKind::Initial => {
                    if initializer.is_some() {
                        return Err(NuError::parse_error(
                            "Duplicate 'initial' block in actor".to_string(),
                            self.current_span(),
                        ));
                    }
                    self.advance(); // consume 'initial'
                    let init_name = self.expect_ident("initializer name")?;
                    self.expect(TokenKind::LParen)?;
                    let params = self.parse_params()?;
                    self.expect(TokenKind::RParen)?;
                    let body = self.parse_expr()?;
                    initializer = Some((init_name, params, body));
                }
                // `version` is a contextual keyword inside entity body
                TokenKind::Ident(ref s) if s == "version" => {
                    self.advance(); // consume 'version'
                    self.expect(TokenKind::Colon)?;
                    let v = self.parse_expr()?;
                    let v_lit = match &v {
                        Expr::Literal(Literal::Int(n), _) => *n as u32,
                        _ => {
                            return Err(NuError::parse_error(
                                "Expected integer literal for version".to_string(),
                                self.current_span(),
                            ));
                        }
                    };
                    version = v_lit;
                }
                // `events` is a contextual keyword inside actor/entity body
                TokenKind::Ident(ref s) if s == "events" => {
                    self.advance(); // consume 'events'
                    if !events.is_empty() {
                        return Err(NuError::parse_error(
                            "Duplicate 'events' block in actor".to_string(),
                            self.current_span(),
                        ));
                    }
                    events = self.parse_events_body()?;
                }
                // `apply` is a contextual keyword inside actor/entity body
                TokenKind::Ident(ref s) if s == "apply" => {
                    self.advance(); // consume 'apply'
                    if !apply_handlers.is_empty() {
                        return Err(NuError::parse_error(
                            "Duplicate 'apply' block in actor".to_string(),
                            self.current_span(),
                        ));
                    }
                    apply_handlers = self.parse_apply_body()?;
                }
                // `migration` is a contextual keyword inside entity body (RFC 0008)
                TokenKind::Ident(ref s) if s == "migration" => {
                    self.advance(); // consume 'migration'
                    migrations.push(self.parse_migration_body()?);
                }
                _ => {
                    return Err(NuError::parse_error(format!(
                            "Expected 'state', 'behavior', 'initial', 'version', 'events', 'apply', or 'migration' in actor body, got {}",
                            self.peek_kind()
                        ), self.current_span()));
                }
            }
        }
        // Post-loop: parse the closing brace.
        self.expect(TokenKind::RBrace)?;

        Ok(Decl::Actor {
            name,
            type_params,
            persistent,
            state_fields,
            behaviors,
            init: vec![],
            backend,
            initializer,
            version,
            events,
            apply_handlers,
            migrations,
            is_organization: is_org,
            virtual_,
            key_params,
            implements,
            span,
        })
    }

    /// Parse an `events` block inside an entity/actor declaration:
    ///
    /// ```text
    ///     | EventName(param: Type, ...)
    ///     | EventName(param: Type, ...)
    /// ```
    fn parse_events_body(&mut self) -> NuResult<Vec<crate::ast::EventDecl>> {
        self.skip_newlines();
        let mut decls = Vec::new();
        while self.consume_if(&TokenKind::Pipe) {
            let span = self.current_span();
            let name = self.expect_ident("event name")?;
            let params = if self.consume_if(&TokenKind::LParen) {
                let p = self.parse_event_params()?;
                self.expect(TokenKind::RParen)?;
                p
            } else {
                vec![]
            };
            decls.push(crate::ast::EventDecl { name, params, span });
            self.skip_newlines();
        }
        if decls.is_empty() {
            return Err(NuError::parse_error(
                "Expected at least one event declaration after 'events'".to_string(),
                self.current_span(),
            ));
        }
        Ok(decls)
    }

    /// Parse an `apply` block inside an entity/actor declaration:
    ///
    /// ```text
    /// apply
    ///     | EventName(param, ...) => body
    ///     | EventName(param, ...) => body
    /// ```
    fn parse_apply_body(&mut self) -> NuResult<Vec<crate::ast::ApplyHandler>> {
        self.skip_newlines();
        let mut handlers = Vec::new();
        while self.consume_if(&TokenKind::Pipe) {
            let span = self.current_span();
            let event = self.expect_ident("event name")?;
            let params = if self.consume_if(&TokenKind::LParen) {
                let p = self.parse_apply_params()?;
                self.expect(TokenKind::RParen)?;
                p
            } else {
                vec![]
            };
            self.expect(TokenKind::FatArrow)?;
            let body = self.parse_expr()?;
            handlers.push(crate::ast::ApplyHandler {
                event,
                params,
                body,
                span,
            });
            self.skip_newlines();
        }
        if handlers.is_empty() {
            return Err(NuError::parse_error(
                "Expected at least one apply handler after 'apply'".to_string(),
                self.current_span(),
            ));
        }
        Ok(handlers)
    }

    /// Parse a `migration` block inside an entity declaration (RFC 0008):
    ///
    /// ```text
    /// migration from <version> to <version> {
    ///     state => { body }
    ///     events {
    ///         | EventName(params) => body
    ///         | other => body
    ///     }
    /// }
    /// ```
    fn parse_migration_body(&mut self) -> NuResult<crate::ast::MigrationDecl> {
        let span = self.current_span();
        let from_name = self.expect_ident("'from' keyword")?;
        if from_name != "from" {
            return Err(NuError::parse_error(
                format!("Expected 'from', got '{}'", from_name),
                self.current_span(),
            ));
        }
        let from_v = self.parse_expr()?;
        let from_version = match &from_v {
            Expr::Literal(Literal::Int(n), _) => *n as u32,
            _ => {
                return Err(NuError::parse_error(
                    "Expected integer literal for migration 'from' version".to_string(),
                    self.current_span(),
                ));
            }
        };
        let to_name = self.expect_ident("'to' keyword")?;
        if to_name != "to" {
            return Err(NuError::parse_error(
                format!("Expected 'to', got '{}'", to_name),
                self.current_span(),
            ));
        }
        let to_v = self.parse_expr()?;
        let to_version = match &to_v {
            Expr::Literal(Literal::Int(n), _) => *n as u32,
            _ => {
                return Err(NuError::parse_error(
                    "Expected integer literal for migration 'to' version".to_string(),
                    self.current_span(),
                ));
            }
        };
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();

        let mut state_body: Option<Expr> = None;
        let mut event_migrations: Vec<(String, Vec<String>, Expr)> = Vec::new();

        while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
            self.skip_newlines();
            if self.match_token(&TokenKind::RBrace) {
                break;
            }
            // `state` is a keyword, not an identifier — handle both cases
            let is_state = self.consume_if(&TokenKind::State);
            if is_state {
                self.expect(TokenKind::FatArrow)?;
                state_body = Some(self.parse_expr()?);
            } else {
                let ident = self.expect_ident("'state' or 'events'")?;
                if ident == "events" {
                    self.expect(TokenKind::LBrace)?;
                    self.skip_newlines();
                    while self.consume_if(&TokenKind::Pipe) {
                        let ev_name = self.expect_ident("event name")?;
                        let ev_params: Vec<String> = if self.consume_if(&TokenKind::LParen) {
                            let mut p = Vec::new();
                            while !self.match_token(&TokenKind::RParen) && !self.is_at_end() {
                                p.push(self.expect_ident("parameter name")?);
                                self.consume_if(&TokenKind::Comma);
                            }
                            self.expect(TokenKind::RParen)?;
                            p
                        } else {
                            vec![]
                        };
                        self.expect(TokenKind::FatArrow)?;
                        let ev_body = self.parse_expr()?;
                        event_migrations.push((ev_name, ev_params, ev_body));
                        self.skip_newlines();
                    }
                    self.expect(TokenKind::RBrace)?;
                } else {
                    return Err(NuError::parse_error(
                        format!(
                            "Expected 'state' or 'events' in migration body, got '{}'",
                            ident
                        ),
                        self.current_span(),
                    ));
                }
            }
            self.skip_newlines();
        }
        self.expect(TokenKind::RBrace)?;

        Ok(crate::ast::MigrationDecl {
            from_version,
            to_version,
            state_body,
            event_migrations,
            span,
        })
    }

    /// Parse event parameters: `name: Type, name: Type, ...`
    fn parse_event_params(&mut self) -> NuResult<Vec<(String, Type)>> {
        let mut params = Vec::new();
        if self.peek_kind() == &TokenKind::RParen {
            return Ok(params);
        }
        loop {
            let name = self.expect_ident("parameter name")?;
            self.expect(TokenKind::Colon)?;
            let ty = self.parse_type()?;
            params.push((name, ty));
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
        }
        Ok(params)
    }

    /// Parse apply handler parameters: just names, no types: `name, name, ...`
    fn parse_apply_params(&mut self) -> NuResult<Vec<String>> {
        let mut params = Vec::new();
        if self.peek_kind() == &TokenKind::RParen {
            return Ok(params);
        }
        loop {
            let name = self.expect_ident("parameter name")?;
            params.push(name);
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
        }
        Ok(params)
    }

    /// Parse a `state_machine` declaration (BEAM_PRIMITIVES §4.2 gen_statem
    /// adaptation, desugared to an actor by [`crate::ast::desugar_state_machine`]):
    ///
    /// ```text
    /// state_machine Name {
    ///   state StateName                       // one or more; first = initial
    ///   event event_name(params): StateName   // target must be a declared state
    ///   on_entry StateName { body }           // hooks; state must be declared
    ///   on_exit StateName { body }
    /// }
    /// ```
    ///
    /// `event`/`on_entry`/`on_exit` are contextual identifiers (like `after`
    /// in `receive ... after`), not reserved keywords. Unlike gen_statem, an
    /// event target MUST be a declared state name — handler-function targets
    /// (e.g. `event data_received(bytes): handle_data` in the §4.2 sketch)
    /// are rejected with a clear error. States must be declared explicitly
    /// with `state` lines, so the aspirational §4.2 sketch parses only once
    /// `Connecting`/`Connected` are declared.
    fn parse_state_machine(&mut self) -> NuResult<Decl> {
        let span = self.current_span();
        self.advance(); // consume 'state_machine'
        let name = self.expect_ident("state_machine name")?;
        self.expect(TokenKind::LBrace)?;

        let mut states: Vec<String> = Vec::new();
        let mut events: Vec<StateMachineEvent> = Vec::new();
        let mut entry_hooks: Vec<(String, Expr)> = Vec::new();
        let mut exit_hooks: Vec<(String, Expr)> = Vec::new();

        self.skip_newlines();
        while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
            self.skip_newlines();
            if self.match_token(&TokenKind::RBrace) {
                break;
            }
            match self.peek_kind().clone() {
                TokenKind::State => {
                    self.advance(); // 'state'
                    states.push(self.expect_ident("state name")?);
                    self.skip_newlines_semicolons();
                }
                TokenKind::Ident(item) => {
                    let tok = self.advance_token();
                    match item.as_str() {
                        "event" => {
                            let event_name = self.expect_ident("event name")?;
                            let params = if self.consume_if(&TokenKind::LParen) {
                                let params = self.parse_params()?;
                                self.expect(TokenKind::RParen)?;
                                params
                            } else {
                                Vec::new()
                            };
                            self.expect(TokenKind::Colon)?;
                            let target = self.expect_ident("event target state")?;
                            events.push(StateMachineEvent {
                                name: event_name,
                                params,
                                target,
                                span: tok.span,
                            });
                            self.skip_newlines_semicolons();
                        }
                        "on_entry" | "on_exit" => {
                            let state_name = self.expect_ident("hook state name")?;
                            let body = self.parse_expr()?;
                            if item == "on_entry" {
                                entry_hooks.push((state_name, body));
                            } else {
                                exit_hooks.push((state_name, body));
                            }
                            self.skip_newlines_semicolons();
                        }
                        other => {
                            return Err(NuError::parse_error(format!(
                                    "Expected 'state', 'event', 'on_entry', or 'on_exit' in state_machine body, got '{}'",
                                    other
                                ), tok.span));
                        }
                    }
                }
                _ => {
                    return Err(NuError::parse_error(format!(
                            "Expected 'state', 'event', 'on_entry', or 'on_exit' in state_machine body, got {}",
                            self.peek_kind()
                        ), self.current_span()));
                }
            }
        }
        self.expect(TokenKind::RBrace)?;

        // Two-pass validation, run only now so `state` lines are known
        // regardless of where they appear relative to events and hooks.
        if states.is_empty() {
            return Err(NuError::parse_error(format!(
                    "state_machine '{}' requires at least one 'state <Name>' declaration (the first declared state is the initial state)",
                    name
                ), span));
        }
        for (i, state) in states.iter().enumerate() {
            if states[..i].contains(state) {
                return Err(NuError::parse_error(
                    format!("duplicate state '{}' in state_machine '{}'", state, name),
                    span,
                ));
            }
        }
        let state_list = states.join(", ");
        let declared = |state: &str| states.iter().any(|s| s == state);
        for (i, event) in events.iter().enumerate() {
            if events[..i].iter().any(|e| e.name == event.name) {
                return Err(NuError::parse_error(
                    format!(
                        "duplicate event '{}' in state_machine '{}'",
                        event.name, name
                    ),
                    event.span,
                ));
            }
            if !declared(&event.target) {
                return Err(NuError::parse_error(format!(
                        "event '{}' targets unknown state '{}' in state_machine '{}' (declared states: {})",
                        event.name, event.target, name, state_list
                    ), event.span));
            }
        }
        for (kind, hooks) in [("on_entry", &entry_hooks), ("on_exit", &exit_hooks)] {
            for (i, (state, _)) in hooks.iter().enumerate() {
                if !declared(state) {
                    return Err(NuError::parse_error(format!(
                            "{} hook references unknown state '{}' in state_machine '{}' (declared states: {})",
                            kind, state, name, state_list
                        ), span));
                }
                if hooks[..i].iter().any(|(s, _)| s == state) {
                    return Err(NuError::parse_error(
                        format!(
                            "duplicate {} hook for state '{}' in state_machine '{}'",
                            kind, state, name
                        ),
                        span,
                    ));
                }
            }
        }

        Ok(Decl::StateMachine {
            name,
            states,
            events,
            entry_hooks,
            exit_hooks,
            span,
        })
    }

    fn parse_agent(&mut self) -> NuResult<Decl> {
        let span = self.current_span();
        self.advance(); // consume 'agent'
        let name = self.expect_ident("agent name")?;
        self.expect(TokenKind::Assign)?;
        self.expect(TokenKind::LBrace)?;

        let mut fallback: Vec<AgentFallbackEntry> = Vec::new();
        let mut retry: Option<AgentRetryConfig> = None;
        let mut model: Option<String> = None;
        let mut system_prompt: Option<String> = None;
        let mut tools: Vec<String> = Vec::new();
        let mut memory: Option<AgentMemoryConfig> = None;
        let mut semantic_memory: Option<AgentSemanticMemoryConfig> = None;
        let mut procedural_memory: Option<AgentProceduralMemoryConfig> = None;
        let mut pricing: Option<AgentPricing> = None;

        self.skip_newlines();
        while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
            self.skip_newlines();
            if self.match_token(&TokenKind::RBrace) {
                break;
            }
            let field_name = self.expect_ident("agent field name")?;
            self.expect(TokenKind::Colon)?;
            match field_name.as_str() {
                "model" => {
                    model = Some(self.expect_string("agent model")?);
                }
                "system_prompt" => {
                    system_prompt = Some(self.expect_string("agent system prompt")?);
                }
                "tools" => {
                    self.expect(TokenKind::LBracket)?;
                    self.skip_newlines();
                    while !self.match_token(&TokenKind::RBracket) && !self.is_at_end() {
                        self.skip_newlines();
                        if self.match_token(&TokenKind::RBracket) {
                            break;
                        }
                        tools.push(self.expect_ident("tool name")?);
                        self.skip_newlines();
                        if !self.consume_if(&TokenKind::Comma) {
                            break;
                        }
                        self.skip_newlines();
                    }
                    self.expect(TokenKind::RBracket)?;
                }
                "memory" => {
                    self.expect(TokenKind::LBrace)?;
                    self.skip_newlines();
                    let mut max_turns: Option<usize> = None;
                    while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
                        self.skip_newlines();
                        if self.match_token(&TokenKind::RBrace) {
                            break;
                        }
                        let mem_field = self.expect_ident("memory field name")?;
                        self.expect(TokenKind::Colon)?;
                        match mem_field.as_str() {
                            "max_turns" => {
                                let n = self.expect_int("max_turns")?;
                                max_turns = Some(n as usize);
                            }
                            other => {
                                return Err(NuError::parse_error(
                                    format!("Unknown memory field: {}", other),
                                    self.current_span(),
                                ));
                            }
                        }
                        self.skip_newlines();
                        if !self.consume_if(&TokenKind::Comma) {
                            break;
                        }
                        self.skip_newlines();
                    }
                    self.expect(TokenKind::RBrace)?;
                    memory = Some(AgentMemoryConfig {
                        max_turns: max_turns.unwrap_or(50),
                    });
                }
                "semantic_memory" => {
                    self.expect(TokenKind::LBrace)?;
                    self.skip_newlines();
                    let mut dimensions: Option<usize> = None;
                    while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
                        self.skip_newlines();
                        if self.match_token(&TokenKind::RBrace) {
                            break;
                        }
                        let sm_field = self.expect_ident("semantic memory field name")?;
                        self.expect(TokenKind::Colon)?;
                        match sm_field.as_str() {
                            "dimensions" => {
                                let n = self.expect_int("dimensions")?;
                                dimensions = Some(n as usize);
                            }
                            other => {
                                return Err(NuError::parse_error(
                                    format!("Unknown semantic_memory field: {}", other),
                                    self.current_span(),
                                ));
                            }
                        }
                        self.skip_newlines();
                        if !self.consume_if(&TokenKind::Comma) {
                            break;
                        }
                        self.skip_newlines();
                    }
                    self.expect(TokenKind::RBrace)?;
                    semantic_memory = Some(AgentSemanticMemoryConfig {
                        dimensions: dimensions.unwrap_or(64),
                    });
                }
                "procedural_memory" => {
                    self.expect(TokenKind::LBrace)?;
                    self.skip_newlines();
                    let mut namespace: Option<String> = None;
                    while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
                        self.skip_newlines();
                        if self.match_token(&TokenKind::RBrace) {
                            break;
                        }
                        let pm_field = self.expect_ident("procedural memory field name")?;
                        self.expect(TokenKind::Colon)?;
                        match pm_field.as_str() {
                            "namespace" => {
                                namespace = Some(self.expect_string("namespace")?);
                            }
                            other => {
                                return Err(NuError::parse_error(
                                    format!("Unknown procedural_memory field: {}", other),
                                    self.current_span(),
                                ));
                            }
                        }
                        self.skip_newlines();
                        if !self.consume_if(&TokenKind::Comma) {
                            break;
                        }
                        self.skip_newlines();
                    }
                    self.expect(TokenKind::RBrace)?;
                    procedural_memory = Some(AgentProceduralMemoryConfig {
                        namespace: namespace.unwrap_or_else(|| "default".to_string()),
                    });
                }
                "pricing" => {
                    self.expect(TokenKind::LBrace)?;
                    self.skip_newlines();
                    let mut input_cost: Option<f64> = None;
                    let mut output_cost: Option<f64> = None;
                    while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
                        self.skip_newlines();
                        if self.match_token(&TokenKind::RBrace) {
                            break;
                        }
                        let price_field = self.expect_ident("pricing field name")?;
                        self.expect(TokenKind::Colon)?;
                        match price_field.as_str() {
                            "input" => {
                                input_cost = Some(self.expect_float("pricing input")?);
                            }
                            "output" => {
                                output_cost = Some(self.expect_float("pricing output")?);
                            }
                            other => {
                                return Err(NuError::parse_error(
                                    format!("Unknown pricing field: {}", other),
                                    self.current_span(),
                                ));
                            }
                        }
                        self.skip_newlines();
                        if !self.consume_if(&TokenKind::Comma) {
                            break;
                        }
                        self.skip_newlines();
                    }
                    self.expect(TokenKind::RBrace)?;
                    pricing = Some(AgentPricing {
                        input: input_cost.unwrap_or(0.0),
                        output: output_cost.unwrap_or(0.0),
                    });
                }
                "fallback" => {
                    self.expect(TokenKind::LBracket)?;
                    self.skip_newlines();
                    while !self.match_token(&TokenKind::RBracket) && !self.is_at_end() {
                        self.skip_newlines();
                        if self.match_token(&TokenKind::RBracket) {
                            break;
                        }
                        self.expect(TokenKind::LBrace)?;
                        self.skip_newlines();
                        let mut fb_model: Option<String> = None;
                        let mut fb_on: Vec<String> = Vec::new();
                        let mut fb_max_tokens: Option<usize> = None;
                        while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
                            self.skip_newlines();
                            if self.match_token(&TokenKind::RBrace) {
                                break;
                            }
                            let fb_field = self.expect_ident("fallback field name")?;
                            self.expect(TokenKind::Colon)?;
                            match fb_field.as_str() {
                                "model" => {
                                    fb_model = Some(self.expect_string("fallback model")?);
                                }
                                "on" => {
                                    self.expect(TokenKind::LBracket)?;
                                    self.skip_newlines();
                                    while !self.match_token(&TokenKind::RBracket)
                                        && !self.is_at_end()
                                    {
                                        self.skip_newlines();
                                        if self.match_token(&TokenKind::RBracket) {
                                            break;
                                        }
                                        fb_on.push(self.expect_ident("error kind")?);
                                        self.skip_newlines();
                                        if !self.consume_if(&TokenKind::Comma) {
                                            break;
                                        }
                                        self.skip_newlines();
                                    }
                                    self.expect(TokenKind::RBracket)?;
                                }
                                "max_tokens" => {
                                    let n = self.expect_int("max_tokens")?;
                                    fb_max_tokens = Some(n as usize);
                                }
                                other => {
                                    return Err(NuError::parse_error(
                                        format!("Unknown fallback field: {}", other),
                                        self.current_span(),
                                    ));
                                }
                            }
                            self.skip_newlines();
                            if !self.consume_if(&TokenKind::Comma) {
                                break;
                            }
                            self.skip_newlines();
                        }
                        self.expect(TokenKind::RBrace)?;
                        let model = fb_model.unwrap_or_default();
                        fallback.push(AgentFallbackEntry {
                            model,
                            on: fb_on,
                            max_tokens: fb_max_tokens,
                        });
                        self.skip_newlines();
                        if !self.consume_if(&TokenKind::Comma) {
                            break;
                        }
                        self.skip_newlines();
                    }
                    self.expect(TokenKind::RBracket)?;
                }
                "retry" => {
                    self.expect(TokenKind::LBrace)?;
                    self.skip_newlines();
                    let mut max_attempts: Option<u32> = None;
                    let mut backoff: Option<AgentBackoff> = None;
                    while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
                        self.skip_newlines();
                        if self.match_token(&TokenKind::RBrace) {
                            break;
                        }
                        let retry_field = self.expect_ident("retry field name")?;
                        self.expect(TokenKind::Colon)?;
                        match retry_field.as_str() {
                            "max_attempts" => {
                                let n = self.expect_int("max_attempts")?;
                                max_attempts = Some(n as u32);
                            }
                            "backoff" => {
                                let name = self.expect_ident("backoff strategy")?;
                                match name.as_str() {
                                    "Exponential" => {
                                        self.expect(TokenKind::LBrace)?;
                                        self.skip_newlines();
                                        let mut initial_ms: Option<u64> = None;
                                        let mut factor: Option<f64> = None;
                                        let mut max_ms: Option<u64> = None;
                                        while !self.match_token(&TokenKind::RBrace)
                                            && !self.is_at_end()
                                        {
                                            self.skip_newlines();
                                            if self.match_token(&TokenKind::RBrace) {
                                                break;
                                            }
                                            let bo_field = self.expect_ident("backoff field")?;
                                            self.expect(TokenKind::Colon)?;
                                            match bo_field.as_str() {
                                                "initial_ms" => {
                                                    initial_ms =
                                                        Some(self.expect_int("initial_ms")? as u64);
                                                }
                                                "factor" => {
                                                    factor = Some(self.expect_float("factor")?);
                                                }
                                                "max_ms" => {
                                                    max_ms =
                                                        Some(self.expect_int("max_ms")? as u64);
                                                }
                                                other => {
                                                    return Err(NuError::parse_error(
                                                        format!(
                                                            "Unknown Exponential backoff field: {}",
                                                            other
                                                        ),
                                                        self.current_span(),
                                                    ));
                                                }
                                            }
                                            self.skip_newlines();
                                            if !self.consume_if(&TokenKind::Comma) {
                                                break;
                                            }
                                            self.skip_newlines();
                                        }
                                        self.expect(TokenKind::RBrace)?;
                                        backoff = Some(AgentBackoff::Exponential {
                                            initial_ms: initial_ms.unwrap_or(200),
                                            factor: factor.unwrap_or(2.0),
                                            max_ms: max_ms.unwrap_or(3000),
                                        });
                                    }
                                    "Fixed" => {
                                        self.expect(TokenKind::LBrace)?;
                                        self.skip_newlines();
                                        let mut delay_ms: Option<u64> = None;
                                        while !self.match_token(&TokenKind::RBrace)
                                            && !self.is_at_end()
                                        {
                                            self.skip_newlines();
                                            if self.match_token(&TokenKind::RBrace) {
                                                break;
                                            }
                                            let field = self.expect_ident("Fixed backoff field")?;
                                            self.expect(TokenKind::Colon)?;
                                            if field == "delay_ms" {
                                                delay_ms =
                                                    Some(self.expect_int("delay_ms")? as u64);
                                            } else {
                                                return Err(NuError::parse_error(
                                                    format!(
                                                        "Unknown Fixed backoff field: {}",
                                                        field
                                                    ),
                                                    self.current_span(),
                                                ));
                                            }
                                            self.skip_newlines();
                                            if !self.consume_if(&TokenKind::Comma) {
                                                break;
                                            }
                                            self.skip_newlines();
                                        }
                                        self.expect(TokenKind::RBrace)?;
                                        backoff = Some(AgentBackoff::Fixed {
                                            delay_ms: delay_ms.unwrap_or(1000),
                                        });
                                    }
                                    other => {
                                        return Err(NuError::parse_error(
                                            format!("Unknown backoff strategy: {}", other),
                                            self.current_span(),
                                        ));
                                    }
                                }
                            }
                            other => {
                                return Err(NuError::parse_error(
                                    format!("Unknown retry field: {}", other),
                                    self.current_span(),
                                ));
                            }
                        }
                        self.skip_newlines();
                        if !self.consume_if(&TokenKind::Comma) {
                            break;
                        }
                        self.skip_newlines();
                    }
                    self.expect(TokenKind::RBrace)?;
                    retry = Some(AgentRetryConfig {
                        max_attempts: max_attempts.unwrap_or(3),
                        backoff: backoff.unwrap_or(AgentBackoff::Exponential {
                            initial_ms: 200,
                            factor: 2.0,
                            max_ms: 3000,
                        }),
                    });
                }
                other => {
                    return Err(NuError::parse_error(
                        format!("Unknown agent field: {}", other),
                        self.current_span(),
                    ));
                }
            }
            self.skip_newlines();
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        self.expect(TokenKind::RBrace)?;

        let model = model.ok_or_else(|| {
            NuError::parse_error(
                "Agent declaration requires a 'model' field".to_string(),
                span,
            )
        })?;

        Ok(Decl::Agent {
            name,
            model,
            system_prompt,
            tools,
            memory: memory.or(Some(AgentMemoryConfig { max_turns: 50 })),
            semantic_memory,
            procedural_memory,
            pricing,
            fallback,
            retry,
            span,
        })
    }

    /// Parse a database declaration:
    /// `database Name { TableName { col: Type modifier*, ... } ... }`
    fn parse_database(&mut self) -> NuResult<Decl> {
        let span = self.current_span();
        self.advance(); // consume 'database'
        let name = self.expect_ident("database name")?;
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();
        let mut tables: Vec<DatabaseTable> = Vec::new();
        while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
            self.skip_newlines();
            if self.match_token(&TokenKind::RBrace) {
                break;
            }
            // Each table: Name { col: Type modifier*, ... }
            let table_name = self.expect_ident("table name")?;
            self.expect(TokenKind::LBrace)?;
            self.skip_newlines();
            let mut columns: Vec<DatabaseColumn> = Vec::new();
            while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
                self.skip_newlines();
                if self.match_token(&TokenKind::RBrace) {
                    break;
                }
                let col_name = self.expect_ident("column name")?;
                self.expect(TokenKind::Colon)?;
                let col_type = self.parse_type()?;
                let mut modifiers: Vec<String> = Vec::new();
                while matches!(
                    self.peek_kind(),
                    TokenKind::Ident(_) | TokenKind::UpperIdent(_)
                ) {
                    let m = self.expect_ident("column modifier")?;
                    modifiers.push(m);
                    self.skip_newlines();
                    if self.match_token(&TokenKind::Comma) || self.match_token(&TokenKind::RBrace) {
                        break;
                    }
                }
                columns.push(DatabaseColumn {
                    name: col_name,
                    col_type,
                    modifiers,
                    span: self.current_span(),
                });
                self.skip_newlines();
                let _ = self.consume_if(&TokenKind::Comma);
                self.skip_newlines();
            }
            self.expect(TokenKind::RBrace)?;
            tables.push(DatabaseTable {
                name: table_name,
                columns,
                span: self.current_span(),
            });
            self.skip_newlines();
        }
        Ok(Decl::Database { name, tables, span })
    }

    /// Parse a CRDT declaration: `crdt Name { type field = value, ... }`
    fn parse_crdt_decl(&mut self) -> NuResult<Decl> {
        let span = self.current_span();
        self.advance(); // consume 'crdt'
        let name = self.expect_ident("crdt name")?;
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();
        let mut fields = Vec::new();
        while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
            self.skip_newlines();
            if self.match_token(&TokenKind::RBrace) {
                break;
            }
            // Parse: type name = default
            let crdt_type = match self.peek_kind() {
                TokenKind::Ident(kw) => {
                    if let Some(ct) = CrdtType::from_keyword(kw) {
                        self.advance(); // consume the type keyword
                        ct
                    } else {
                        return Err(NuError::parse_error(
                            format!("Unknown CRDT type: {}", kw),
                            self.current_span(),
                        ));
                    }
                }
                _ => {
                    return Err(NuError::parse_error(
                        "Expected CRDT type (gcounter, pncounter, gset, orset, aworset, lwwregister, mvregister, rga)".to_string(),
                        self.current_span(),
                    ));
                }
            };
            let field_name = self.expect_ident("field name")?;
            self.expect(TokenKind::Colon)?;
            let ty = self.parse_type()?;
            self.expect(TokenKind::Assign)?;
            let default = self.parse_expr()?;
            fields.push((field_name, crdt_type, ty, default));
            self.skip_newlines();
            let _ = self.consume_if(&TokenKind::Comma);
            self.skip_newlines();
        }
        self.expect(TokenKind::RBrace)?;
        Ok(Decl::CrdtDecl { name, fields, span })
    }

    fn parse_state_model(&mut self, default_model: StateModel) -> StateModel {
        match self.peek_kind() {
            TokenKind::Local => {
                self.advance();
                StateModel::Local
            }
            TokenKind::Durable => {
                self.advance();
                StateModel::Durable
            }
            TokenKind::EventSourced => {
                self.advance();
                StateModel::EventSourced
            }
            TokenKind::Crdt => {
                self.advance(); // consume 'crdt'
                                // Optional CRDT type: gcounter, pncounter, gset, orset, etc.
                let crdt_type = match self.peek_kind() {
                    TokenKind::Ident(name) => {
                        if let Some(ct) = CrdtType::from_keyword(name) {
                            self.advance(); // consume the type keyword
                            ct
                        } else {
                            CrdtType::default() // not a CRDT type, probably field name
                        }
                    }
                    _ => CrdtType::default(),
                };
                StateModel::Crdt(crdt_type)
            }
            _ => default_model,
        }
    }

    fn parse_workflow(&mut self) -> NuResult<Decl> {
        let span = self.current_span();
        self.advance(); // consume 'workflow'
        let name = self.expect_ident("workflow name")?;
        self.expect(TokenKind::LBrace)?;

        let mut items = Vec::new();
        let mut compensate = None;

        self.skip_newlines();
        while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
            self.skip_newlines();
            if self.match_token(&TokenKind::RBrace) {
                break;
            }
            match self.peek_kind() {
                TokenKind::Step => {
                    items.push(WorkflowItem::Step(self.parse_workflow_step()?));
                }
                TokenKind::Parallel => {
                    self.advance(); // 'parallel'
                    self.expect(TokenKind::LBrace)?;
                    let mut branch = Vec::new();
                    self.skip_newlines();
                    while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
                        self.skip_newlines();
                        if self.match_token(&TokenKind::RBrace) {
                            break;
                        }
                        branch.push(self.parse_workflow_step()?);
                        self.skip_newlines_semicolons();
                    }
                    self.expect(TokenKind::RBrace)?;
                    items.push(WorkflowItem::Parallel(branch));
                    self.skip_newlines_semicolons();
                }
                TokenKind::Compensate => {
                    self.advance(); // 'compensate'
                    self.expect(TokenKind::LBrace)?;
                    self.skip_newlines();
                    compensate = Some(self.parse_expr()?);
                    self.skip_newlines();
                    self.expect(TokenKind::RBrace)?;
                    self.skip_newlines_semicolons();
                }
                _ => {
                    return Err(NuError::parse_error(
                        format!(
                            "Expected 'step', 'parallel', or 'compensate' in workflow body, got {}",
                            self.peek_kind()
                        ),
                        self.current_span(),
                    ));
                }
            }
        }
        self.expect(TokenKind::RBrace)?;

        Ok(Decl::Workflow {
            name,
            input: None,
            items,
            compensate,
            span,
        })
    }

    fn parse_workflow_step(&mut self) -> NuResult<WorkflowStep> {
        let span = self.current_span();
        self.expect(TokenKind::Step)?;
        let name = self.expect_ident("step name")?;
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();
        let body = self.parse_expr()?;
        self.skip_newlines();
        self.expect(TokenKind::RBrace)?;
        let compensate = if self.consume_if(&TokenKind::Compensate) {
            self.expect(TokenKind::LBrace)?;
            self.skip_newlines();
            let expr = self.parse_expr()?;
            self.skip_newlines();
            self.expect(TokenKind::RBrace)?;
            Some(expr)
        } else {
            None
        };
        Ok(WorkflowStep {
            name,
            body,
            compensate,
            span,
        })
    }

    fn parse_type_alias(&mut self, public: bool, opaque: bool) -> NuResult<Decl> {
        let span = self.current_span();
        if !opaque {
            self.advance(); // consume 'alias'
        }
        let name = self.expect_ident("type alias name")?;
        let type_params = self.parse_type_params()?;
        self.expect(TokenKind::Assign)?;
        let body = self.parse_type()?;
        Ok(Decl::TypeAlias {
            name,
            type_params,
            body,
            opaque,
            public,
            span,
        })
    }

    fn parse_type_decl_variant_or_record(
        &mut self,
        public: bool,
        derives: Vec<String>,
    ) -> NuResult<Decl> {
        let span = self.current_span();
        let name = self.expect_ident("type name")?;
        let type_params = self.parse_type_params()?;
        self.expect(TokenKind::Assign)?;

        // Look ahead to determine if it's a record, variant, or alias body.
        self.skip_newlines();
        match self.peek_kind().clone() {
            TokenKind::LBrace => {
                // Record type
                self.advance(); // '{'
                let fields = self.parse_record_type_fields()?;
                Ok(Decl::RecordType {
                    name,
                    type_params,
                    fields,
                    derives,
                    public,
                    span,
                })
            }
            // Variants start with a variant name (UpperIdent) or an optional
            // leading pipe. Any other shape is an alias body (`type Buffer =
            // [Int]`): parse the full type like `type alias`. Primitive type
            // names (`Int`, `String`, ...) lex as UpperIdent, so they are
            // excluded from the variant path. `Nil` is the one exception: the
            // canonical empty variant of a sum type.
            TokenKind::UpperIdent(first) if type_decl_body_is_alias(&first) => {
                let body = self.parse_type()?;
                Ok(Decl::TypeAlias {
                    name,
                    type_params,
                    body,
                    opaque: false,
                    public,
                    span,
                })
            }
            TokenKind::UpperIdent(_) | TokenKind::Pipe => {
                let variants = self.parse_variants()?;
                Ok(Decl::VariantType {
                    name,
                    type_params,
                    variants,
                    public,
                    span,
                })
            }
            _ => {
                let body = self.parse_type()?;
                Ok(Decl::TypeAlias {
                    name,
                    type_params,
                    body,
                    opaque: false,
                    public,
                    span,
                })
            }
        }
    }

    fn parse_effect_decl(&mut self) -> NuResult<Decl> {
        let span = self.current_span();
        self.advance(); // consume 'effect'
        let name = self.expect_ident("effect name")?;
        self.expect(TokenKind::LBrace)?;

        let mut ops = Vec::new();
        self.skip_newlines();
        while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
            self.skip_newlines();
            if self.match_token(&TokenKind::RBrace) {
                break;
            }
            let op_name = self.expect_ident("operation name")?;
            self.expect(TokenKind::Colon)?;

            // Parse argument types
            // Forms: op: A -> B (single arg), op: (A, B) -> C (multiple args), op: -> B (no args)
            let mut arg_types = Vec::new();
            if self.consume_if(&TokenKind::LParen) {
                // Multi-arg form: op: (A, B) -> C
                while !self.match_token(&TokenKind::RParen) && !self.is_at_end() {
                    arg_types.push(self.parse_type()?);
                    if !self.consume_if(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(TokenKind::RParen)?;
            } else if !self.match_token(&TokenKind::Arrow) {
                // Single arg form: op: A -> B
                arg_types.push(self.parse_type_atomic()?);
            }
            // else: no-arg form op: -> B

            self.expect(TokenKind::Arrow)?;
            let ret_type = self.parse_type()?;
            ops.push((op_name, arg_types, ret_type));
            self.skip_newlines_semicolons();
        }
        self.expect(TokenKind::RBrace)?;

        Ok(Decl::EffectDecl { name, ops, span })
    }

    fn parse_named_handler(&mut self) -> NuResult<Decl> {
        let span = self.current_span();
        self.advance();
        let name = self.expect_ident("handler name")?;
        self.expect(TokenKind::Assign)?;
        self.expect(TokenKind::LBrace)?;
        let mut handlers = Vec::new();
        self.skip_newlines();
        while self.peek_kind() != &TokenKind::RBrace && !self.is_at_end() {
            self.consume_if(&TokenKind::Pipe);
            let effect_name = self.expect_ident("effect name")?;
            self.expect(TokenKind::Dot)?;
            let op_name = self.expect_ident("operation name")?;
            self.expect(TokenKind::LParen)?;
            let mut params = Vec::new();
            self.skip_newlines();
            while self.peek_kind() != &TokenKind::RParen && !self.is_at_end() {
                params.push(self.expect_ident("param name")?);
                self.skip_newlines();
                if !self.consume_if(&TokenKind::Comma) {
                    break;
                }
                self.skip_newlines();
            }
            self.expect(TokenKind::RParen)?;
            let has_resume = self.consume_if(&TokenKind::Resume);
            self.expect(TokenKind::FatArrow)?;
            let handler_body = self.parse_expr()?;
            handlers.push(EffectHandler {
                effect_name,
                op_name,
                params,
                body: handler_body,
                resume: has_resume,
            });
            self.skip_newlines_semicolons();
        }
        self.expect(TokenKind::RBrace)?;
        self.handler_registry.insert(name.clone(), handlers.clone());
        Ok(Decl::NamedHandler {
            name,
            handlers,
            span,
        })
    }

    fn parse_class(&mut self) -> NuResult<Decl> {
        let span = self.current_span();
        self.advance(); // consume 'class'
        let name = self.expect_ident("class name")?;
        // Type parameters [T, U] or [T: Ord]
        let (type_params, type_param_constraints) = self.parse_type_params_with_constraints()?;
        // Optional superclass: `class Ord[T]: Eq[T]`
        let super_classes = if self.consume_if(&TokenKind::Colon) {
            let mut supers = Vec::new();
            supers.push(self.expect_ident("superclass name")?);
            while self.consume_if(&TokenKind::Plus) {
                supers.push(self.expect_ident("superclass name")?);
            }
            supers
        } else {
            Vec::new()
        };
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();
        let mut methods = Vec::new();
        while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
            self.skip_newlines();
            if self.match_token(&TokenKind::RBrace) {
                break;
            }
            // Method: `fn name(self: T, x: U) -> R` or `fn name(self: T, x: U) -> R = body`
            self.expect(TokenKind::Fn)?;
            let method_name = self.expect_ident("method name")?;
            self.expect(TokenKind::LParen)?;
            // Parse method params, accepting `self` as a special parameter name
            let params = self.parse_method_params()?;
            self.expect(TokenKind::RParen)?;
            let return_type = if self.consume_if(&TokenKind::Arrow) {
                self.parse_type()?
            } else {
                Type::unit()
            };
            // Optional default body: `= expr`
            let default_body = if self.consume_if(&TokenKind::Assign) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            let typed_params: Vec<(String, Type)> = params
                .iter()
                .map(|p| (p.name.clone(), p.ty.clone().unwrap_or(Type::unit())))
                .collect();
            methods.push(ClassMethod {
                name: method_name,
                params: typed_params,
                return_type,
                default_body,
            });
            self.skip_newlines_semicolons();
        }
        self.expect(TokenKind::RBrace)?;
        Ok(Decl::Class {
            name,
            type_params,
            type_param_constraints,
            super_classes,
            methods,
            span,
        })
    }

    fn parse_impl(&mut self) -> NuResult<Decl> {
        let span = self.current_span();
        self.advance(); // consume 'impl'
        let class_name = self.expect_ident("class name")?;
        // Type parameters [T, U]
        let type_params = self.parse_type_params()?;
        let for_type = self.parse_type()?;
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();
        let mut methods = Vec::new();
        while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
            self.skip_newlines();
            if self.match_token(&TokenKind::RBrace) {
                break;
            }
            // Method: `fn name(self, x) = expr`
            self.expect(TokenKind::Fn)?;
            let method_name = self.expect_ident("method name")?;
            self.expect(TokenKind::LParen)?;
            let params = self.parse_method_params()?;
            self.expect(TokenKind::RParen)?;
            let return_type = if self.consume_if(&TokenKind::Arrow) {
                self.parse_type()?
            } else {
                Type::unit()
            };
            self.expect(TokenKind::Assign)?;
            let body = self.parse_expr()?;
            let typed_params: Vec<(String, Type)> = params
                .iter()
                .map(|p| (p.name.clone(), p.ty.clone().unwrap_or(Type::unit())))
                .collect();
            methods.push(ImplMethod {
                name: method_name,
                params: typed_params,
                return_type,
                body,
            });
            self.skip_newlines_semicolons();
        }
        self.expect(TokenKind::RBrace)?;
        Ok(Decl::Impl {
            class_name,
            type_params,
            for_type,
            methods,
            span,
        })
    }

    fn parse_import(&mut self) -> NuResult<Decl> {
        let span = self.current_span();
        self.advance(); // consume 'import'

        // Parse import path:
        //   - stdlib::set::... or stdlib::web::types
        //   - @nulang/auth or @nulang/auth/session
        //   - plain ident (relative file)
        let path = if self.consume_if(&TokenKind::At) {
            let mut p = "@".to_string();
            p.push_str(&self.expect_ident("module name")?);
            while self.consume_if(&TokenKind::Slash) {
                p.push('/');
                p.push_str(&self.expect_ident("module name")?);
            }
            p
        } else {
            let mut p = self.expect_ident("import path")?;
            while self.consume_if(&TokenKind::DoubleColon) {
                p.push_str("::");
                p.push_str(&self.expect_ident("module name")?);
            }
            p
        };

        // Helpful error for dot-separated imports (e.g. `import stdlib.list`)
        if self.match_token(&TokenKind::Dot) {
            return Err(NuError::parse_error(
                "expected `::` in import path; use `::` to separate path segments (not `.`)"
                    .to_string(),
                self.current_span(),
            ));
        }
        let items = Vec::new();
        self.skip_newlines_semicolons();
        Ok(Decl::Import { path, items, span })
    }

    /// Parse a module-level let binding: `let name [: Type] = value`
    fn parse_module_let(&mut self, _public: bool) -> NuResult<Decl> {
        let span = self.current_span();
        let saved_pos = self.pos;
        self.advance(); // consume 'let'
        self.skip_newlines();
        // `rec` is accepted for backward compatibility but is a no-op:
        // self-recursion is inferred automatically for any lambda-valued
        // `let` binding (see infer_letrec / the Expr::Lambda check in
        // typechecker.rs), matching the same acceptance already present
        // for expression-position `let` (see the TokenKind::Let arm in
        // parse_primary's `let` handling).
        self.consume_if(&TokenKind::Rec);
        self.skip_newlines();
        let mutable = self.consume_if(&TokenKind::Var);
        self.skip_newlines();
        let name = self.expect_ident("binding name")?;
        self.skip_newlines();
        // Function-parameter binding (`let rec f(x) = ... in ...` or the
        // plain `let f(x) = ... in ...` form) is EXPRESSION-position
        // syntax: rewind to the `let` token and signal a zero-consumption
        // error so `parse_module`'s expression fallback routes it through
        // `parse_primary`'s let arm → `parse_let_rec_named`. Without this,
        // the `(` after the name fails here with "Expected =" and the
        // module parser (consumed > 0) never reaches the expression path.
        if self.peek_kind() == &TokenKind::LParen {
            self.pos = saved_pos;
            return Err(NuError::parse_error(
                "let with function parameters is expression-position syntax".to_string(),
                span,
            ));
        }
        let type_ann = if self.consume_if(&TokenKind::Colon) {
            self.skip_newlines();
            Some(self.parse_type()?)
        } else {
            None
        };
        self.skip_newlines();
        self.expect(TokenKind::Assign)?;
        self.skip_newlines();
        let value = self.parse_expr()?;
        self.skip_newlines();
        // Check if this is a let-in expression (has a body after `in`).
        // If so, rewind and let the expression path handle it.
        if self.peek_kind() == &TokenKind::In {
            self.pos = saved_pos;
            return Err(NuError::parse_error(
                "let-in at module level".to_string(),
                span,
            ));
        }
        self.skip_newlines_semicolons();
        Ok(Decl::LetBinding {
            name,
            type_ann,
            value,
            mutable,
            span,
        })
    }

    /// Parse a module-level given binding: `given name [: Type] = value`
    fn parse_given(&mut self, _public: bool) -> NuResult<Decl> {
        let span = self.current_span();
        self.advance(); // consume 'given'
        self.skip_newlines();
        let name = self.expect_ident("given name")?;
        self.skip_newlines();
        let ty = if self.consume_if(&TokenKind::Colon) {
            self.skip_newlines();
            Some(self.parse_type()?)
        } else {
            None
        };
        self.skip_newlines();
        self.expect(TokenKind::Assign)?;
        self.skip_newlines();
        let value = self.parse_expr()?;
        Ok(Decl::Given {
            name,
            ty,
            value,
            span,
        })
    }

    fn parse_signal(&mut self) -> NuResult<Decl> {
        let span = self.current_span();
        self.advance(); // consume 'signal'
        let name = self.expect_ident("signal name")?;
        self.expect(TokenKind::Colon)?;
        let ty = self.parse_type()?;
        self.expect(TokenKind::Assign)?;
        let init = self.parse_expr()?;
        Ok(Decl::Signal {
            name,
            ty,
            init,
            span,
        })
    }

    fn parse_extern(&mut self, _public: bool) -> NuResult<Decl> {
        let span = self.current_span();
        self.advance(); // consume 'extern'

        let library = match self.peek_kind() {
            TokenKind::StringLit(s) => {
                let s = s.clone();
                self.advance();
                s
            }
            other => {
                return Err(NuError::parse_error(
                    format!(
                        "Expected string literal for library path, found {:?}",
                        other
                    ),
                    self.current_span(),
                ))
            }
        };

        self.expect(TokenKind::LBrace)?;

        let mut funcs = Vec::new();
        self.skip_newlines();
        while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
            self.skip_newlines();
            if self.match_token(&TokenKind::RBrace) {
                break;
            }

            let func_span = self.current_span();
            self.expect(TokenKind::Fn)?;
            let name = self.expect_ident("function name")?;
            self.expect(TokenKind::LParen)?;
            let raw_params = self.parse_params()?;
            self.expect(TokenKind::RParen)?;

            // Extern parameters must have explicit types.
            let mut params = Vec::new();
            for p in raw_params {
                match p.ty {
                    Some(ty) => params.push((p.name, ty)),
                    None => {
                        return Err(NuError::parse_error(
                            format!(
                                "Extern function '{}' parameter '{}' requires an explicit type",
                                name, p.name
                            ),
                            func_span,
                        ))
                    }
                }
            }

            self.expect(TokenKind::Arrow)?;
            let ret = self.parse_type()?;

            funcs.push(ExternFunc {
                name,
                params,
                ret,
                span: func_span,
            });
            self.skip_newlines_semicolons();
        }

        self.expect(TokenKind::RBrace)?;
        Ok(Decl::Extern {
            library,
            funcs,
            span,
        })
    }

    // === Behaviors ===

    fn parse_behavior(&mut self) -> NuResult<Behavior> {
        let span = self.current_span();
        self.advance(); // consume 'behavior'
        let name = self.expect_ident("behavior name")?;
        self.expect(TokenKind::LParen)?;
        let params = self.parse_params()?;
        self.expect(TokenKind::RParen)?;

        // Optional effect annotation
        // Optional return type annotation
        let ret_type = if self.consume_if(&TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };

        // Optional effect annotation
        let effect = if self.consume_if(&TokenKind::Bang) || self.consume_if(&TokenKind::Throws) {
            Some(self.parse_effect_row()?)
        } else {
            None
        };

        // Optional capability annotation
        let cap = if self.consume_if(&TokenKind::Colon) {
            self.parse_capability()?
        } else {
            Capability::Ref // default
        };

        let body = self.parse_expr()?;
        Ok(Behavior {
            name,
            params,
            body,
            effect,
            cap,
            ret_type,
            span,
        })
    }

    // === Expressions (Pratt parser) ===

    fn parse_expr(&mut self) -> NuResult<Expr> {
        self.parse_expr_with_prec(PREC_LOWEST)
    }

    fn parse_expr_with_prec(&mut self, min_prec: u8) -> NuResult<Expr> {
        // Parse prefix / primary expression
        let mut left = self.parse_prefix()?;

        // Handle infix operators
        loop {
            self.skip_newlines();
            let op = self.peek_kind().clone();
            if op == TokenKind::Eof {
                break;
            }

            // Special cases: function call, field access, array index, send
            if self.match_token(&TokenKind::LParen) {
                // Intercept `Grain("Type", key)` before treating it as a call.
                if let Expr::Var(ref name, _) = left {
                    if name == "Grain" {
                        self.advance(); // consume '('
                        let args = self.parse_arg_list()?;
                        if args.len() != 2 {
                            return Err(NuError::parse_error(
                                format!(
                                    "Grain(...) expects exactly 2 arguments, got {}",
                                    args.len()
                                ),
                                self.current_span(),
                            ));
                        }
                        let grain_type = match &args[0] {
                            Expr::Literal(Literal::String(s), _) => s.clone(),
                            _ => {
                                return Err(NuError::parse_error(
                                    "Grain(...) first argument must be a string literal"
                                        .to_string(),
                                    self.current_span(),
                                ))
                            }
                        };
                        left = Expr::GrainRef {
                            grain_type,
                            key: Box::new(args[1].clone()),
                            span: self.current_span(),
                        };
                        continue;
                    }
                }
                // Function call: left(args)
                self.advance(); // consume '('
                let args = self.parse_arg_list()?;
                let span = self.current_span();
                left = Expr::App {
                    func: Box::new(left),
                    args,
                    span,
                };
                continue;
            }

            // Send: actor ! behavior(args)
            if self.consume_if(&TokenKind::Bang) {
                let behavior = self.expect_ident("behavior name")?;
                self.expect(TokenKind::LParen)?;
                let args = self.parse_arg_list()?;
                let span = self.current_span();
                left = Expr::Send {
                    actor: Box::new(left),
                    behavior,
                    args,
                    remote: false,
                    span,
                };
                continue;
            }

            // Async tell: actor <- behavior(args)  (same semantics as `!`)
            if self.consume_if(&TokenKind::ThinArrow) {
                let behavior = self.expect_ident("behavior name")?;
                self.expect(TokenKind::LParen)?;
                let args = self.parse_arg_list()?;
                let span = self.current_span();
                left = Expr::Send {
                    actor: Box::new(left),
                    behavior,
                    args,
                    remote: false,
                    span,
                };
                continue;
            }

            // Async ask: actor <-? behavior(args)  (same semantics as `ask`)
            if self.consume_if(&TokenKind::ThinArrowQuestion) {
                let behavior = self.expect_ident("behavior name")?;
                self.expect(TokenKind::LParen)?;
                let args = self.parse_arg_list()?;
                let span = self.current_span();
                left = Expr::Ask {
                    actor: Box::new(left),
                    behavior,
                    args,
                    remote: false,
                    timeout_ms: None,
                    span,
                };
                continue;
            }

            if self.consume_if(&TokenKind::Dot) {
                // Field access: expr.field, expr.0, expr.0.1
                let span_start = self.current_span();
                match self.peek_kind().clone() {
                    TokenKind::IntLit(n) => {
                        self.advance();
                        left = Expr::FieldAccess {
                            expr: Box::new(left),
                            field: format!("{}", n),
                            span: span_start,
                        };
                    }
                    TokenKind::FloatLit(_v) => {
                        // For chained tuple access like p.0.1 or p.0.0,
                        // the lexer produces e.g. 0.1 or 0.0 as a single
                        // float token.  Advance and use the token span to
                        // recover the original source text (format!("{}")
                        // drops trailing zeros, losing the distinction
                        // between 0.0 and 0).
                        let tok = self.advance_token();
                        let source_text =
                            crate::types::source_slice_for_span(tok.span).unwrap_or_default();
                        let parts: Vec<&str> = source_text.split('.').collect();
                        if let Some((first, rest)) = parts.split_first() {
                            left = Expr::FieldAccess {
                                expr: Box::new(left),
                                field: first.to_string(),
                                span: span_start,
                            };
                            for part in rest {
                                left = Expr::FieldAccess {
                                    expr: Box::new(left),
                                    field: part.to_string(),
                                    span: span_start,
                                };
                            }
                        }
                    }
                    _ => {
                        let field = self.expect_ident("field name")?;
                        left = Expr::FieldAccess {
                            expr: Box::new(left),
                            field,
                            span: span_start,
                        };
                    }
                }
                continue;
            }

            // Optional chaining: `expr?.field` — nil-safe field access.
            // Desugars to: match expr { nil => nil, __tmp => __tmp.field }
            if self.peek_kind() == &TokenKind::Question {
                // Check if it's `?.` (Question followed by Dot)
                let saved = self.pos;
                self.advance(); // consume '?'
                if self.peek_kind() == &TokenKind::Dot {
                    self.advance(); // consume '.'
                    let span_start = self.current_span();
                    let field = self.expect_ident("field name")?;
                    let tmp = format!("__q{}", span_start.start);
                    let match_span = self.current_span();
                    left = Expr::Match {
                        scrutinee: Box::new(left),
                        arms: vec![
                            (
                                Pattern::Lit(Literal::Nil),
                                None,
                                Expr::Literal(Literal::Nil, match_span),
                            ),
                            (
                                Pattern::Var(tmp.clone()),
                                None,
                                Expr::FieldAccess {
                                    expr: Box::new(Expr::Var(tmp, match_span)),
                                    field,
                                    span: span_start,
                                },
                            ),
                        ],
                        span: match_span,
                    };
                    continue;
                }
                // Not `?.` — restore and fall through to `?` error propagation
                self.pos = saved;
            }
            // Try operator: expr? desugars to match on Ok/Error
            if self.consume_if(&TokenKind::Question) {
                let span = self.current_span();
                let x = "__try_x".to_string();
                let e = "__try_e".to_string();
                left = Expr::Match {
                    scrutinee: Box::new(left),
                    arms: vec![
                        (
                            Pattern::Variant(
                                "Ok".to_string(),
                                Some(Box::new(Pattern::Var(x.clone()))),
                            ),
                            None,
                            Expr::Var(x, span),
                        ),
                        (
                            Pattern::Variant(
                                "Error".to_string(),
                                Some(Box::new(Pattern::Var(e.clone()))),
                            ),
                            None,
                            Expr::Return(
                                Some(Box::new(Expr::App {
                                    func: Box::new(Expr::Var("Error".to_string(), span)),
                                    args: vec![Expr::Var(e, span)],
                                    span,
                                })),
                                span,
                            ),
                        ),
                    ],
                    span,
                };
                continue;
            }

            // Catch expression: expr catch fallback_expr
            // Desugars to match expr { Ok(x) => x, Error(_) => fallback }
            if self.consume_if(&TokenKind::Catch) {
                let span = self.current_span();
                self.warnings.push(NuWarning::deprecated_catch(span));
                if self.consume_if(&TokenKind::LBrace) {
                    // Block form: expr catch { | pat => body, ... }
                    let mut arms = Vec::new();
                    let x = "__catch_x".to_string();
                    // Ok arm: unwrap the success value
                    arms.push((
                        Pattern::Variant("Ok".to_string(), Some(Box::new(Pattern::Var(x.clone())))),
                        None,
                        Expr::Var(x, span),
                    ));
                    // Parse user-provided Error arms
                    self.skip_newlines();
                    while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
                        self.skip_newlines();
                        if self.match_token(&TokenKind::RBrace) {
                            break;
                        }
                        // Optional leading `|` before each arm
                        self.consume_if(&TokenKind::Pipe);
                        self.skip_newlines();
                        let pat = self.parse_pattern()?;
                        let guard = if self.consume_if(&TokenKind::If) {
                            Some(self.parse_expr()?)
                        } else {
                            None
                        };
                        self.expect(TokenKind::FatArrow)?;
                        let body = self.parse_expr()?;
                        arms.push((pat, guard, body));
                        self.skip_newlines_semicolons();
                        self.consume_if(&TokenKind::Comma);
                    }
                    self.expect(TokenKind::RBrace)?;
                    left = Expr::Match {
                        scrutinee: Box::new(left),
                        arms,
                        span,
                    };
                } else {
                    // Bare form: expr catch fallback_expr
                    let fallback = self.parse_expr()?;
                    let x = "__catch_x".to_string();
                    left = Expr::Match {
                        scrutinee: Box::new(left),
                        arms: vec![
                            (
                                Pattern::Variant(
                                    "Ok".to_string(),
                                    Some(Box::new(Pattern::Var(x.clone()))),
                                ),
                                None,
                                Expr::Var(x, span),
                            ),
                            (
                                Pattern::Variant(
                                    "Error".to_string(),
                                    Some(Box::new(Pattern::Wild)),
                                ),
                                None,
                                fallback,
                            ),
                        ],
                        span,
                    };
                }
                continue;
            }
            if self.consume_if(&TokenKind::LBracket) {
                // Array index: arr[idx]
                let idx = self.parse_expr()?;
                self.expect(TokenKind::RBracket)?;
                let span = self.current_span();
                left = Expr::Index {
                    arr: Box::new(left),
                    idx: Box::new(idx),
                    span,
                };
                continue;
            }

            if self.consume_if(&TokenKind::Colon) {
                let span = self.current_span();
                let is_cap = if let TokenKind::Ident(ref s) = self.peek_kind() {
                    s == "cap"
                } else {
                    false
                };
                if is_cap {
                    self.advance(); // consume 'cap'
                    let cap = self.parse_capability()?;
                    left = Expr::CapAnnotate {
                        expr: Box::new(left),
                        cap,
                        span,
                    };
                } else {
                    let ty = self.parse_type()?;
                    left = Expr::TypeAnnotate {
                        expr: Box::new(left),
                        ty,
                        span,
                    };
                }
                continue;
            }

            // Check for infix operators
            let (prec, right_assoc) = match infix_precedence(&op) {
                Some(p) => p,
                None => break,
            };

            if prec < min_prec {
                break;
            }

            self.advance(); // consume operator
            let next_min_prec = if right_assoc { prec } else { prec + 1 };
            let right = self.parse_expr_with_prec(next_min_prec)?;

            let span = self.current_span();
            if op == TokenKind::PipeOp {
                left = Expr::Pipe {
                    left: Box::new(left),
                    right: Box::new(right),
                    span,
                };
                continue;
            }

            let bin_op = token_to_binop(&op).ok_or_else(|| {
                NuError::parse_error(format!("Not a binary operator: {:?}", op), span.clone())
            })?;

            left = Expr::Binary {
                op: bin_op,
                left: Box::new(left),
                right: Box::new(right),
                span,
            };
        }

        Ok(left)
    }

    fn parse_prefix(&mut self) -> NuResult<Expr> {
        self.skip_newlines();
        let span = self.current_span();

        match self.peek_kind().clone() {
            TokenKind::Eof => Err(NuError::parse_error(
                "Unexpected end of file in expression".to_string(),
                span,
            )),
            kind => {
                // Check for prefix operators
                if let Some((prec, _)) = prefix_precedence(&kind) {
                    self.advance(); // consume operator
                                    // For `&`, check for an optional capability keyword
                                    // immediately after: &val, &ref, &iso, etc.
                                    // Bare `&` defaults to `&ref` (backward compatible).
                    let ref_cap = if matches!(kind, TokenKind::Ampersand) {
                        match self.peek_kind() {
                            TokenKind::Val
                            | TokenKind::Ref
                            | TokenKind::Iso
                            | TokenKind::Trn
                            | TokenKind::Box
                            | TokenKind::Tag
                            | TokenKind::LinearIso
                            | TokenKind::Linear => self.parse_capability()?,
                            _ => Capability::Ref,
                        }
                    } else {
                        Capability::Ref
                    };
                    let operand = self.parse_expr_with_prec(prec)?;
                    let span = self.current_span();
                    let un_op = match kind {
                        TokenKind::Minus => UnOp::Neg,
                        TokenKind::Not | TokenKind::Bang => UnOp::Not,
                        TokenKind::Ampersand => UnOp::Ref(ref_cap),
                        TokenKind::Star => UnOp::Deref,
                        _ => unreachable!(),
                    };
                    return Ok(Expr::Unary {
                        op: un_op,
                        expr: Box::new(operand),
                        span,
                    });
                }

                match kind {
                    // Literals
                    TokenKind::IntLit(_)
                    | TokenKind::FloatLit(_)
                    | TokenKind::StringLit(_)
                    | TokenKind::FStringLit(_)
                    | TokenKind::BoolLit(_)
                    | TokenKind::NilLit
                    | TokenKind::UnitLit => self.parse_literal(),

                    // `after` as a standalone temporal expression:
                    // `after ms => body` desugars to `receive {} after ms => body`.
                    // `after` is a contextual keyword — special only in expression
                    // prefix position; it remains a usable identifier when not
                    // followed by `expr => body`.
                    TokenKind::Ident(name) if name == "after" => {
                        let saved = self.pos;
                        self.advance(); // consume 'after'
                                        // Peek: is the next token expression-like?
                                        // `after => ...` is invalid (missing timeout), so don't try.
                        let looks_like_after = self.peek_kind() != &TokenKind::FatArrow
                            && self.peek_kind() != &TokenKind::Eof
                            && self.is_expr_start();
                        if looks_like_after {
                            if let Ok(timeout) = self.parse_expr() {
                                if self.consume_if(&TokenKind::FatArrow) {
                                    let body = self.parse_expr()?;
                                    let after_span = Span::new(span.start, self.current_span().end);
                                    return Ok(Expr::Receive {
                                        arms: vec![],
                                        after: Some((Box::new(timeout), Box::new(body))),
                                        span: after_span,
                                    });
                                }
                            }
                        }
                        // Not a valid `after expr => body` — restore cursor to
                        // just after `after` so the caller's Pratt loop can
                        // handle `after(args)`, `after.field`, etc.
                        self.pos = saved + 1;
                        Ok(Expr::Var("after".to_string(), span))
                    }
                    // `dbg(expr)` contextual keyword — desugars to `perform Debug.dbg(expr)`
                    TokenKind::Ident(name) if name == "dbg" => {
                        self.advance(); // consume 'dbg'
                        if self.consume_if(&TokenKind::LParen) {
                            let expr = self.parse_expr()?;
                            self.expect(TokenKind::RParen)?;
                            Ok(Expr::Perform {
                                effect: "Debug".to_string(),
                                op: "dbg".to_string(),
                                args: vec![expr],
                                span,
                            })
                        } else {
                            // Not dbg(expr) — treat as variable reference
                            Ok(Expr::Var("dbg".to_string(), span))
                        }
                    }
                    TokenKind::Ident(name) => {
                        let name = name.clone();
                        self.advance();
                        // Check for assignment
                        if self.consume_if(&TokenKind::Assign) {
                            let val = self.parse_expr()?;
                            let span = self.current_span();
                            Ok(Expr::Assign {
                                target: Box::new(Expr::Var(name, span.clone())),
                                value: Box::new(val),
                                span,
                            })
                        } else {
                            Ok(Expr::Var(name, span))
                        }
                    }
                    TokenKind::UpperIdent(name) => {
                        let name = name.clone();
                        self.advance();
                        Ok(Expr::Var(name, span))
                    }

                    // Keywords that start expressions
                    TokenKind::Fn => self.parse_lambda(),
                    TokenKind::Let => {
                        self.advance();
                        self.skip_newlines();
                        self.consume_if(&TokenKind::Rec);
                        // `let _ = expr [in body]` — wildcard binding, desugar to discard
                        if self.peek_kind() == &TokenKind::Ident("_".to_string()) {
                            self.advance(); // consume '_'
                            let _ty = if self.consume_if(&TokenKind::Colon) {
                                Some(self.parse_type()?)
                            } else {
                                None
                            };
                            self.expect(TokenKind::Assign)?;
                            let value = self.parse_expr()?;
                            if self.consume_if(&TokenKind::In) {
                                let body = self.parse_expr()?;
                                Ok(Expr::Block {
                                    exprs: vec![value, body],
                                    span: self.current_span(),
                                })
                            } else {
                                // Statement-let: `let _ = expr;` — just the expr for effects
                                Ok(value)
                            }
                        } else {
                            let name = self.expect_ident("variable name")?;
                            if self.peek_kind() == &TokenKind::LParen {
                                self.parse_let_rec_named(name)
                            } else {
                                self.parse_let_named(name, false)
                            }
                        }
                    }
                    TokenKind::Var => {
                        self.advance();
                        self.skip_newlines();
                        let name = self.expect_ident("variable name")?;
                        self.parse_let_named(name, true)
                    }
                    TokenKind::If => self.parse_if(),
                    TokenKind::Match => self.parse_match(),
                    TokenKind::LBrace => {
                        // Disambiguation: { expr .. field = val } (record-update)
                        // vs { expr .. expr } (block containing a range expression)
                        // vs { ident : val } (record literal) vs { stmt; ... } (block).
                        //
                        // `..` has precedence PREC_RANGE (3). We call
                        // parse_expr_with_prec(PREC_RANGE+1) so the Pratt loop
                        // does NOT consume `..`, letting us check manually
                        // whether the next token after the right operand is `=`
                        // (record-update) or something else (range in block).
                        let saved = self.pos;
                        self.advance(); // consume '{'
                        self.skip_newlines();
                        if let Ok(expr) = self.parse_expr_with_prec(PREC_RANGE + 1) {
                            self.skip_newlines();
                            if self.consume_if(&TokenKind::DotDot) {
                                // Peek: record-update is `{ base .. field = val }`
                                // Range-in-block is `{ base .. expr }` (no `=` after right side).
                                // Try parsing just a field name; if followed by `=`,
                                // it's record-update. Otherwise fall through to block.
                                let after_dotdot = self.pos;
                                if let Ok(field_name) = self.expect_ident("field name") {
                                    self.skip_newlines();
                                    if self.peek_kind() == &TokenKind::Assign {
                                        // Record-update: { base .. field = value, ... }
                                        self.advance(); // consume '='
                                        let val = self.parse_expr()?;
                                        let mut fields = vec![(field_name, val)];
                                        self.skip_newlines();
                                        while self.consume_if(&TokenKind::Comma) {
                                            self.skip_newlines();
                                            let field = self.expect_ident("field name")?;
                                            self.expect(TokenKind::Assign)?;
                                            let val = self.parse_expr()?;
                                            fields.push((field, val));
                                            self.skip_newlines();
                                        }
                                        self.expect(TokenKind::RBrace)?;
                                        return Ok(Expr::RecordUpdate {
                                            base: Box::new(expr),
                                            fields,
                                            span,
                                        });
                                    }
                                }
                                // Not record-update: restore to just after `..`
                                // and fall through to block parsing.
                                self.pos = after_dotdot;
                            }
                        }
                        // Not a record-update — restore and fall through
                        self.pos = saved;
                        if self.is_record_literal_ahead() {
                            self.parse_record_literal()
                        } else {
                            self.parse_block()
                        }
                    }
                    TokenKind::Par => self.parse_par(),
                    TokenKind::LParen => self.parse_tuple_or_paren(),
                    TokenKind::LBracket => self.parse_array(),
                    TokenKind::Spawn => self.parse_spawn(),
                    TokenKind::Send => self.parse_send_keyword(),
                    TokenKind::Ask => self.parse_ask(),
                    TokenKind::Catch => self.parse_catch_prefix(),
                    TokenKind::Perform => self.parse_perform(),
                    TokenKind::Resume => self.parse_resume(),
                    TokenKind::Handle => self.parse_handle(),
                    TokenKind::Emit => self.parse_emit(),
                    TokenKind::Receive => self.parse_receive(),
                    TokenKind::For => self.parse_for(),
                    TokenKind::While => self.parse_while(),
                    TokenKind::Until => self.parse_until(),
                    TokenKind::Migrate => self.parse_migrate(),
                    TokenKind::With => self.parse_with_block(),
                    TokenKind::Consume => self.parse_consume_expr(),
                    TokenKind::Recover => self.parse_recover_expr(),
                    TokenKind::Defer => self.parse_defer_expr(false),
                    TokenKind::Errdefer => self.parse_defer_expr(true),
                    TokenKind::Hide => self.parse_hide_expr(false),
                    TokenKind::Seal => self.parse_hide_expr(true),
                    TokenKind::Return => {
                        self.advance();
                        if self.is_expr_start() {
                            let val = self.parse_expr()?;
                            Ok(Expr::Return(Some(Box::new(val)), self.current_span()))
                        } else {
                            Ok(Expr::Return(None, self.current_span()))
                        }
                    }
                    TokenKind::Fail => {
                        let fspan = self.current_span();
                        self.advance();
                        // fail is sugar for return; same semantics
                        self.warnings.push(NuWarning::deprecated_fail(fspan));
                        if self.is_expr_start() {
                            let val = self.parse_expr()?;
                            Ok(Expr::Return(Some(Box::new(val)), self.current_span()))
                        } else {
                            Ok(Expr::Return(None, self.current_span()))
                        }
                    }
                    TokenKind::Break => {
                        self.advance();
                        if self.is_expr_start() {
                            let val = self.parse_expr()?;
                            Ok(Expr::Break(Some(Box::new(val)), self.current_span()))
                        } else {
                            Ok(Expr::Break(None, self.current_span()))
                        }
                    }
                    TokenKind::SelfKw => self.parse_self_ref(),
                    TokenKind::Lt => self.parse_html_expr(),

                    _ => Err(NuError::parse_error(
                        format!("Unexpected token in expression: {}", kind),
                        span,
                    )),
                }
            }
        }
    }

    // === Expression Primitives ===

    fn parse_literal(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        match self.peek_kind().clone() {
            TokenKind::IntLit(v) => {
                self.advance();
                Ok(Expr::Literal(Literal::Int(v), span))
            }
            TokenKind::FloatLit(v) => {
                self.advance();
                Ok(Expr::Literal(Literal::Float(v), span))
            }
            TokenKind::StringLit(s) => {
                let s = s.clone();
                self.advance();
                if s.contains("#{") {
                    self.parse_interpolated_string(&s, "#{", span)
                } else {
                    Ok(Expr::Literal(Literal::String(s), span))
                }
            }
            TokenKind::FStringLit(s) => {
                let s = s.clone();
                self.advance();
                if s.contains("{") {
                    self.parse_interpolated_string(&s, "{", span)
                } else {
                    Ok(Expr::Literal(Literal::String(s), span))
                }
            }
            TokenKind::BoolLit(b) => {
                self.advance();
                Ok(Expr::Literal(Literal::Bool(b), span))
            }
            TokenKind::NilLit => {
                self.advance();
                Ok(Expr::Literal(Literal::Nil, span))
            }
            TokenKind::UnitLit => {
                self.advance();
                Ok(Expr::Literal(Literal::Unit, span))
            }
            _ => Err(NuError::parse_error("Expected literal".to_string(), span)),
        }
    }

    fn parse_lambda(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance(); // consume 'fn'
        self.expect(TokenKind::LParen)?;
        let params = self.parse_params()?;
        self.expect(TokenKind::RParen)?;

        // Optional return type: fn(x) -> RetType { body }
        let ret_type = if self.consume_if(&TokenKind::Arrow) {
            // Look ahead: if the next token is '{', this arrow introduces
            // the body (bare arrow syntax); otherwise it's a return type.
            if self.match_token(&TokenKind::LBrace) {
                None
            } else {
                Some(self.parse_type()?)
            }
        } else {
            None
        };

        let body = self.parse_expr()?;
        Ok(Expr::Lambda {
            params,
            ret_type,
            body: Box::new(body),
            effect: None,
            span,
        })
    }

    fn parse_let_named(&mut self, name: String, mutable: bool) -> NuResult<Expr> {
        let span = self.current_span();

        // Optional type annotation
        let ty = if self.consume_if(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };

        self.expect(TokenKind::Assign)?;
        let value = self.parse_expr()?;
        let (body, let_in) = if self.consume_if(&TokenKind::In) {
            (self.parse_expr()?, true)
        } else {
            (
                Expr::Block {
                    exprs: vec![],
                    span: Span::default(),
                },
                false,
            )
        };
        Ok(Expr::Let {
            name,
            ty,
            value: Box::new(value),
            body: Box::new(body),
            mutable,
            let_in,
            span,
        })
    }

    /// Parse a string containing `#{...}` interpolation markers.
    fn parse_interpolated_string(&self, raw: &str, marker: &str, span: Span) -> NuResult<Expr> {
        let mut parts: Vec<Expr> = Vec::new();
        let mut remaining = raw;
        while let Some(hash_brace) = remaining.find(marker) {
            if hash_brace > 0 {
                parts.push(Expr::Literal(
                    Literal::String(remaining[..hash_brace].to_string()),
                    span,
                ));
            }
            let expr_start = hash_brace + marker.len();
            let expr_str = &remaining[expr_start..];
            let mut depth = 1u32;
            let mut expr_end = 0usize;
            for (i, ch) in expr_str.char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            expr_end = i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if depth != 0 {
                return Err(NuError::parse_error(
                    "Unterminated interpolation: missing '}'".to_string(),
                    span,
                ));
            }
            let expr_content = &expr_str[..expr_end];
            let expr = self.parse_inline_expr(expr_content, span)?;
            parts.push(expr);
            remaining = &expr_str[expr_end + 1..];
        }
        if !remaining.is_empty() {
            parts.push(Expr::Literal(Literal::String(remaining.to_string()), span));
        }
        if parts.len() == 1 {
            return Ok(parts.into_iter().next().unwrap());
        }
        if parts.len() == 1 {
            return Ok(parts.into_iter().next().unwrap());
        }
        Ok(Expr::FString(parts, span))
    }

    fn parse_inline_expr(&self, source: &str, span: Span) -> NuResult<Expr> {
        let mut lexer = crate::lexer::Lexer::new(source);
        let tokens = lexer.lex().map_err(|e| {
            NuError::parse_error(format!("Invalid interpolation expression: {}", e), span)
        })?;
        let mut sub_parser = Parser::new(tokens);
        sub_parser.parse_expr()
    }

    fn parse_let_rec_named(&mut self, name: String) -> NuResult<Expr> {
        let span = self.current_span();
        self.expect(TokenKind::LParen)?;
        let params = self.parse_params()?;
        self.expect(TokenKind::RParen)?;
        self.expect(TokenKind::Assign)?;
        let value = self.parse_expr()?;
        self.expect(TokenKind::In)?;
        let body = self.parse_expr()?;
        Ok(Expr::LetRec {
            name,
            params,
            value: Box::new(value),
            body: Box::new(body),
            span,
        })
    }

    fn parse_if(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance(); // consume 'if'

        // `if let <pattern> = <scrutinee> { <body> } [else { <fallback> }]`
        // Desugars to `match <scrutinee> { <pattern> => <body>, _ => <fallback-or-unit> }`
        if self.consume_if(&TokenKind::Let) {
            let pat = self.parse_pattern()?;
            self.expect(TokenKind::Assign)?;
            let scrutinee = self.parse_expr()?;
            let then_branch = self.parse_block()?;
            self.skip_newlines();
            let else_branch = if self.consume_if(&TokenKind::Else) {
                Some(if self.match_token(&TokenKind::LBrace) {
                    self.parse_block()?
                } else {
                    self.parse_expr()?
                })
            } else {
                None
            };
            let wildcard_arm = (
                Pattern::Wild,
                None,
                else_branch.unwrap_or(Expr::Literal(Literal::Unit, span)),
            );
            let then_arm = (pat, None, then_branch);
            return Ok(Expr::Match {
                scrutinee: Box::new(scrutinee),
                arms: vec![then_arm, wildcard_arm],
                span,
            });
        }

        // Detect C-style parenthesized condition: `if (cond) ...`
        let cstyle_paren = self.peek_kind() == &TokenKind::LParen;

        let cond = self.parse_expr()?;

        // If user wrote `if (cond) ...` C-style, suggest nulang syntax
        if cstyle_paren {
            let next = self.peek_kind();
            if next == &TokenKind::LBrace || next == &TokenKind::Then {
                return Err(NuError::parse_error(
                    "Nulang uses `if cond then body else other` (no parentheses around the condition)".to_string(),
                    span,
                ));
            }
        }

        // Optional `then` keyword for ML-style syntax: `if c then a else b`
        let has_then = self.consume_if(&TokenKind::Then);

        // Parse then branch: either { block } or single expression
        let then_branch = if self.match_token(&TokenKind::LBrace) {
            if !has_then {
                return Err(NuError::parse_error(
                    "expected `then` after if-condition; use `if <cond> then <body> else <else>`"
                        .to_string(),
                    self.current_span(),
                ));
            }
            Box::new(self.parse_block()?)
        } else {
            Box::new(self.parse_expr()?)
        };

        self.skip_newlines();
        let else_branch = if self.consume_if(&TokenKind::Else) {
            Some(if self.match_token(&TokenKind::LBrace) {
                Box::new(self.parse_block()?)
            } else {
                Box::new(self.parse_expr()?)
            })
        } else {
            None
        };

        Ok(Expr::If {
            cond: Box::new(cond),
            then_branch,
            else_branch,
            span,
        })
    }

    fn parse_match(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance(); // consume 'match'
        let scrutinee = self.parse_expr()?;
        let _ = self.consume_if(&TokenKind::With); // `with` is optional
        self.expect(TokenKind::LBrace)?;

        let mut arms = Vec::new();
        self.skip_newlines();
        while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
            self.skip_newlines();
            if self.match_token(&TokenKind::RBrace) {
                break;
            }

            // Optional leading `case` or `|` before each arm.
            let _ = self.consume_if(&TokenKind::Case);
            if self.consume_if(&TokenKind::Pipe) {
                // OK
            }
            self.skip_newlines();

            let pat = self.parse_pattern()?;
            // Or-patterns: `| A(x) | B(x) => body` — parse additional
            // alternatives and desugar by duplicating the arm for each.
            let mut alt_pats = vec![pat];
            self.skip_newlines();
            while self.consume_if(&TokenKind::Pipe) {
                self.skip_newlines();
                alt_pats.push(self.parse_pattern()?);
                self.skip_newlines();
            }
            // Optional guard: `| pat if cond => body`. The guard is a full
            // expression; it may reference variables bound by the pattern.
            let guard = if self.consume_if(&TokenKind::If) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            self.expect(TokenKind::FatArrow)?;
            let expr = self.parse_expr()?;
            for p in alt_pats {
                arms.push((p, guard.clone(), expr.clone()));
            }
            self.skip_newlines_semicolons();
            self.consume_if(&TokenKind::Comma);
        }
        self.expect(TokenKind::RBrace)?;

        Ok(Expr::Match {
            scrutinee: Box::new(scrutinee),
            arms,
            span,
        })
    }

    fn parse_block(&mut self) -> NuResult<Expr> {
        let _span = self.current_span();
        self.advance(); // consume '{'
        let exprs = self.collect_block_exprs(Some(TokenKind::RBrace))?;
        self.expect(TokenKind::RBrace)?;
        Ok(Expr::Block {
            exprs,
            span: self.current_span(),
        })
    }

    /// Parse a `par { e1; e2; ... }` block — an independence annotation
    /// (see `Expr::Par`). The sub-expressions are evaluated in order, just
    /// like a `Block`; the distinct node lets later passes exploit the
    /// declared independence. Mirrors `parse_block`.
    fn parse_par(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance(); // consume 'par'
        self.skip_newlines();
        if !self.match_token(&TokenKind::LBrace) {
            return Err(NuError::parse_error(
                "expected '{' after 'par'".to_string(),
                self.current_span(),
            ));
        }
        self.advance(); // consume '{'
        let exprs = self.collect_block_exprs(Some(TokenKind::RBrace))?;
        self.expect(TokenKind::RBrace)?;
        Ok(Expr::Par { exprs, span })
    }

    /// Collect expressions until `end_token` (or EOF), splicing incomplete
    /// let-bindings so that `let x = 1` captures following expressions as
    /// its body. Handles chains of statement-lets iteratively.
    fn collect_block_exprs(&mut self, end_token: Option<TokenKind>) -> NuResult<Vec<Expr>> {
        let mut exprs = Vec::new();
        self.skip_newlines();
        while !self.is_at_end() {
            if let Some(ref end) = end_token {
                if self.match_token(end) {
                    break;
                }
            }
            self.skip_newlines();
            if self.is_at_end() {
                break;
            }
            if let Some(ref end) = end_token {
                if self.match_token(end) {
                    break;
                }
            }
            let expr = self.parse_expr()?;
            self.skip_newlines_semicolons();

            let is_incomplete = matches!(&expr, Expr::Let { body, .. } | Expr::LetRec { body, .. } if matches!(body.as_ref(), Expr::Block { exprs, span } if exprs.is_empty() && span.start == 0 && span.end == 0));

            if is_incomplete {
                // Collect consecutive incomplete lets iteratively to avoid
                // deep recursion for blocks with many sequential let-statements
                // (e.g. 40 let bindings produce 40 AST levels).  Once the chain
                // of consecutive lets ends, recurse *once* to collect the rest
                // (which may itself start another chain).  This keeps recursion
                // depth proportional to the number of let-*chains*, not the
                // number of individual let-statements.
                let mut pending: Vec<Expr> = vec![expr];
                let final_body = loop {
                    if let Some(ref end) = end_token {
                        if self.match_token(end) {
                            break Expr::Literal(Literal::Unit, Span::default());
                        }
                    }
                    self.skip_newlines();
                    if self.is_at_end() {
                        break Expr::Literal(Literal::Unit, Span::default());
                    }
                    if let Some(ref end) = end_token {
                        if self.match_token(end) {
                            break Expr::Literal(Literal::Unit, Span::default());
                        }
                    }
                    let next = self.parse_expr()?;
                    self.skip_newlines_semicolons();
                    let next_incomplete = matches!(
                        &next,
                        Expr::Let { body, .. } | Expr::LetRec { body, .. }
                        if matches!(body.as_ref(), Expr::Block { exprs, span }
                            if exprs.is_empty() && span.start == 0 && span.end == 0)
                    );
                    if next_incomplete {
                        pending.push(next);
                    } else {
                        // First complete expression: collect the rest
                        // (including any trailing lets) via one recursive
                        // call so they are properly nested.
                        let mut rest = vec![next];
                        rest.extend(self.collect_block_exprs(end_token.clone())?);
                        let body = if rest.len() == 1 {
                            rest.into_iter().next().unwrap()
                        } else {
                            Expr::Block {
                                exprs: rest,
                                span: Span::default(),
                            }
                        };
                        break body;
                    }
                };
                // Fold: Let(a0, 0, Let(a1, 1, ... final_body))
                let mut body = final_body;
                for let_expr in pending.into_iter().rev() {
                    body = match let_expr {
                        Expr::Let {
                            name,
                            ty,
                            value,
                            mutable,
                            span,
                            ..
                        } => Expr::Let {
                            name,
                            ty,
                            value,
                            mutable,
                            body: Box::new(body),
                            let_in: false,
                            span,
                        },
                        Expr::LetRec {
                            name,
                            params,
                            value,
                            span,
                            ..
                        } => Expr::LetRec {
                            name,
                            params,
                            value,
                            body: Box::new(body),
                            span,
                        },
                        _ => unreachable!(),
                    };
                }
                exprs.push(body);
                break;
            }
            exprs.push(expr);
        }
        Ok(exprs)
    }

    fn parse_tuple_or_paren(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance(); // consume '('
        self.skip_newlines();

        // Empty paren = unit
        if self.consume_if(&TokenKind::RParen) {
            return Ok(Expr::Literal(Literal::Unit, span));
        }

        let first = self.parse_expr()?;
        self.skip_newlines();

        // Single paren = grouped expr
        if self.consume_if(&TokenKind::RParen) {
            return Ok(first);
        }

        // Tuple: (e1, e2, ...)
        let mut elems = vec![first];
        while self.consume_if(&TokenKind::Comma) {
            self.skip_newlines();
            if self.match_token(&TokenKind::RParen) {
                break;
            }
            elems.push(self.parse_expr()?);
            self.skip_newlines();
        }
        self.expect(TokenKind::RParen)?;
        Ok(Expr::Tuple(elems, span))
    }

    fn parse_spawn(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance(); // consume 'spawn'
                        // Optional `link`/`monitor` modifier (BEAM spawn_link/spawn_monitor).
                        // `spawn link A { ... }` desugars right here to
                        // `let __spawn_ref = spawn A { ... } in { perform Actor.link(__spawn_ref); __spawn_ref }`
                        // (likewise `Actor.monitor`), so the form typechecks exactly like a
                        // plain spawn (actor ref) and needs no new IR nodes or opcodes.
        let link_op = match self.peek_kind() {
            TokenKind::Link => {
                self.advance();
                Some("link")
            }
            TokenKind::Monitor => {
                self.advance();
                Some("monitor")
            }
            _ => None,
        };

        // Optional remote target: `spawn@target_expr Foo(...)`.
        let target_node = if self.consume_if(&TokenKind::At) {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        // Parse the actor name.  In a spawn expression the target is always a
        // simple name (like `Counter` or `DurableCounter`), never an arbitrary
        // expression.  We parse it as an identifier so `spawn Foo(args)` does
        // not get misinterpreted as a function call.
        let actor_name = match self.peek_kind().clone() {
            TokenKind::Ident(s) | TokenKind::UpperIdent(s) => {
                self.advance();
                s
            }
            _ => {
                return Err(NuError::parse_error(
                    format!("Expected actor name in spawn, got {}", self.peek_kind()),
                    self.current_span(),
                ))
            }
        };
        let actor_type = Expr::Var(actor_name, span);

        // Optional positional constructor args: `spawn Foo(a, b)`
        let positional_args = if self.peek_kind() == &TokenKind::LParen {
            self.advance(); // consume '('
            let mut args = Vec::new();
            self.skip_newlines();
            while !self.match_token(&TokenKind::RParen) && !self.is_at_end() {
                args.push(self.parse_expr()?);
                self.skip_newlines();
                if !self.consume_if(&TokenKind::Comma) {
                    break;
                }
                self.skip_newlines();
            }
            self.expect(TokenKind::RParen)?;
            Some(args)
        } else {
            None
        };

        // Field init block `{ field = val, ... }` — required if no positional args.
        let init = if positional_args.is_none() {
            self.expect(TokenKind::LBrace)?;
            let mut fields = Vec::new();
            self.skip_newlines();
            while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
                self.skip_newlines();
                if self.match_token(&TokenKind::RBrace) {
                    break;
                }
                let field = self.expect_ident("field name")?;
                if !self.consume_if(&TokenKind::Assign) {
                    self.expect(TokenKind::Colon)?;
                }
                let val = self.parse_expr()?;
                fields.push((field, val));
                self.skip_newlines_semicolons();
            }
            self.expect(TokenKind::RBrace)?;
            fields
        } else {
            Vec::new()
        };

        // Optional named registration: `spawn Foo() as "name"`
        let register_as = if self.consume_if(&TokenKind::As) {
            Some(self.expect_string("actor name")?)
        } else {
            None
        };
        let spawned = Expr::Spawn {
            actor_type: Box::new(actor_type),
            init,
            positional_args,
            register_as,
            target_node,
            span,
        };
        Ok(match link_op {
            None => spawned,
            Some(op) => {
                let t = "__spawn_ref".to_string();
                Expr::Let {
                    name: t.clone(),
                    ty: None,
                    value: Box::new(spawned),
                    body: Box::new(Expr::Block {
                        exprs: vec![
                            Expr::Perform {
                                effect: "Actor".to_string(),
                                op: op.to_string(),
                                args: vec![Expr::Var(t.clone(), span)],
                                span,
                            },
                            Expr::Var(t, span),
                        ],
                        span,
                    }),
                    mutable: false,
                    span,
                    let_in: false,
                }
            }
        })
    }

    fn parse_send_keyword(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance(); // consume 'send'
        let remote = self.consume_if(&TokenKind::Remote);
        let actor = self.parse_expr()?;
        let behavior = self.expect_ident("behavior name")?;
        self.expect(TokenKind::LParen)?;
        let args = self.parse_arg_list()?;
        Ok(Expr::Send {
            actor: Box::new(actor),
            behavior,
            args,
            remote,
            span,
        })
    }

    fn parse_ask(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance(); // consume 'ask'
        let remote = self.consume_if(&TokenKind::Remote);
        let actor = self.parse_expr()?;
        // Allow the behavior name to be `ask` itself so agent actors can expose
        // an `ask(prompt)` behavior callable as `ask a ask("...")`.
        let behavior = match self.peek_kind() {
            TokenKind::Ask => {
                self.advance();
                "ask".to_string()
            }
            _ => self.expect_ident("behavior name")?,
        };
        self.expect(TokenKind::LParen)?;
        let args = self.parse_arg_list()?;
        // Optional transport modifiers: `timeout <int>`
        let timeout_ms = if let TokenKind::Ident(ref s) = self.peek_kind() {
            if s == "timeout" {
                self.advance(); // consume 'timeout'
                let ms = self.parse_expr()?;
                // Evaluate timeout at compile time if it's a literal int
                if let Expr::Literal(Literal::Int(n), _) = &ms {
                    Some(*n as u64)
                } else {
                    None // dynamic timeout not yet supported
                }
            } else {
                None
            }
        } else {
            None
        };
        Ok(Expr::Ask {
            actor: Box::new(actor),
            behavior,
            args,
            remote,
            timeout_ms,
            span,
        })
    }

    fn parse_resume(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance();
        self.expect(TokenKind::LParen)?;
        let value = self.parse_expr()?;
        self.expect(TokenKind::RParen)?;
        Ok(Expr::Resume {
            value: Box::new(value),
            span,
        })
    }

    fn parse_perform(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance(); // consume 'perform'
        let effect = self.expect_ident("effect name")?;
        self.expect(TokenKind::Dot)?;
        // `ask`, `link`, `monitor`, `exit` and `ref` are reserved keywords,
        // so they lex as keyword tokens rather than identifiers; accept them
        // as operation names (`perform Actor.link(t)`, `perform Grain.ref(...)`).
        let op = match self.peek_kind() {
            TokenKind::Ask => {
                self.advance();
                "ask".to_string()
            }
            TokenKind::Link => {
                self.advance();
                "link".to_string()
            }
            TokenKind::Monitor => {
                self.advance();
                "monitor".to_string()
            }
            TokenKind::Exit => {
                self.advance();
                "exit".to_string()
            }
            TokenKind::Ref => {
                self.advance();
                "ref".to_string()
            }
            _ => self.expect_ident("operation name")?,
        };
        self.expect(TokenKind::LParen)?;
        let args = self.parse_arg_list()?;
        Ok(Expr::Perform {
            effect,
            op,
            args,
            span,
        })
    }

    fn parse_emit(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance(); // consume 'emit'
        let event = self.expect_ident("event name")?;
        self.expect(TokenKind::LParen)?;
        let args = self.parse_arg_list()?;
        Ok(Expr::Emit { event, args, span })
    }

    // -----------------------------------------------------------------------
    // JSX / HTML expressions (desugared to `el(tag, attrs, children)`)
    // -----------------------------------------------------------------------

    /// True for tokens that are reserved keywords (anything that is not a
    /// literal, identifier, operator, or delimiter). Used by JSX parsing so
    /// keywords like `class` can appear as tag/attribute names.
    fn is_keyword_token(kind: &TokenKind) -> bool {
        match kind {
            TokenKind::IntLit(_)
            | TokenKind::FloatLit(_)
            | TokenKind::StringLit(_)
            | TokenKind::FStringLit(_)
            | TokenKind::BoolLit(_)
            | TokenKind::NilLit
            | TokenKind::UnitLit
            | TokenKind::Ident(_)
            | TokenKind::UpperIdent(_)
            | TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::Star
            | TokenKind::Star2
            | TokenKind::Slash
            | TokenKind::Percent
            | TokenKind::Eq
            | TokenKind::Ne
            | TokenKind::Lt
            | TokenKind::Le
            | TokenKind::Gt
            | TokenKind::Ge
            | TokenKind::And
            | TokenKind::Or
            | TokenKind::Not
            | TokenKind::Ampersand
            | TokenKind::Pipe
            | TokenKind::PipeOp
            | TokenKind::Pipe3
            | TokenKind::Caret
            | TokenKind::Tilde
            | TokenKind::Shl
            | TokenKind::Shr
            | TokenKind::Assign
            | TokenKind::PlusAssign
            | TokenKind::MinusAssign
            | TokenKind::Arrow
            | TokenKind::FatArrow
            | TokenKind::ThinArrow
            | TokenKind::ThinArrowQuestion
            | TokenKind::Dot
            | TokenKind::DotDot
            | TokenKind::Colon
            | TokenKind::DoubleColon
            | TokenKind::At
            | TokenKind::Bang
            | TokenKind::Question
            | TokenKind::LParen
            | TokenKind::RParen
            | TokenKind::LBrace
            | TokenKind::RBrace
            | TokenKind::LBracket
            | TokenKind::RBracket
            | TokenKind::Comma
            | TokenKind::Semicolon
            | TokenKind::Newline
            | TokenKind::Comment(_)
            | TokenKind::DocComment(_)
            | TokenKind::Eof => false,
            _ => true,
        }
    }

    fn expect_html_ident(&mut self, msg: &str) -> NuResult<String> {
        let span = self.current_span();
        match self.peek_kind().clone() {
            TokenKind::Ident(s) | TokenKind::UpperIdent(s) => {
                self.advance();
                Ok(s)
            }
            other if Self::is_keyword_token(&other) => {
                self.advance();
                Ok(other.to_string())
            }
            other => Err(NuError::parse_error(
                format!("Expected {}, found {}", msg, other),
                span,
            )),
        }
    }

    /// Parse a JSX/HTML element: `<tag attrs>children</tag>` or `<tag attrs />`.
    /// Desugars to `el("tag", attrs, children)` where `attrs` and `children`
    /// are arrays. Text children are wrapped in `text("...")` calls.
    fn parse_html_expr(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.expect(TokenKind::Lt)?; // consume '<'
        let tag = self.expect_html_ident("HTML tag name")?;

        let mut attrs: Vec<Expr> = Vec::new();
        loop {
            self.skip_newlines();
            if self.consume_if(&TokenKind::Slash) {
                self.expect(TokenKind::Gt)?;
                return Ok(self.html_el(&tag, attrs, Vec::new(), span));
            }
            if self.consume_if(&TokenKind::Gt) {
                let children = self.parse_html_children(&tag, span)?;
                return Ok(self.html_el(&tag, attrs, children, span));
            }

            // Attribute: name="value" or name={expr} or boolean name
            let attr_name = self.expect_html_ident("HTML attribute name")?;
            let attr_val = if self.consume_if(&TokenKind::Assign) {
                if self.consume_if(&TokenKind::LBrace) {
                    let expr = self.parse_expr()?;
                    self.expect(TokenKind::RBrace)?;
                    expr
                } else {
                    let s = self.expect_string("HTML attribute value")?;
                    self.html_text(&s, span)
                }
            } else {
                self.html_text(&attr_name, span)
            };
            attrs.push(Expr::Tuple(
                vec![Expr::Literal(Literal::String(attr_name), span), attr_val],
                span,
            ));
        }
    }

    fn html_el(&self, tag: &str, attrs: Vec<Expr>, children: Vec<Expr>, span: Span) -> Expr {
        Expr::App {
            func: Box::new(Expr::Var("el".to_string(), span)),
            args: vec![
                Expr::Literal(Literal::String(tag.to_string()), span),
                Expr::Array(attrs, span),
                Expr::Array(children, span),
            ],
            span,
        }
    }

    fn html_text(&self, s: &str, span: Span) -> Expr {
        Expr::App {
            func: Box::new(Expr::Var("text".to_string(), span)),
            args: vec![Expr::Literal(Literal::String(s.to_string()), span)],
            span,
        }
    }

    fn parse_html_children(&mut self, close_tag: &str, span: Span) -> NuResult<Vec<Expr>> {
        let mut children: Vec<Expr> = Vec::new();
        while !self.is_at_end() {
            self.skip_newlines();

            // Closing tag: </close_tag>
            if self.peek_kind() == &TokenKind::Lt {
                let saved = self.pos;
                self.advance(); // '<'
                if self.consume_if(&TokenKind::Slash) {
                    let closing = self.expect_html_ident("closing tag name")?;
                    if closing == close_tag {
                        self.expect(TokenKind::Gt)?;
                        return Ok(children);
                    } else {
                        return Err(NuError::parse_error(
                            format!(
                                "Expected closing tag </{}>, found </{}>",
                                close_tag, closing
                            ),
                            self.current_span(),
                        ));
                    }
                }
                // Not a closing tag: restore and parse nested element
                self.pos = saved;
                children.push(self.parse_html_expr()?);
                continue;
            }

            // Interpolated expression: {expr}
            if self.consume_if(&TokenKind::LBrace) {
                let expr = self.parse_expr()?;
                self.expect(TokenKind::RBrace)?;
                children.push(expr);
                continue;
            }

            if self.peek_kind() == &TokenKind::Eof {
                break;
            }

            // Text node: consume the next token as literal text
            let text = self.html_token_text()?;
            children.push(self.html_text(&text, span));
        }
        Err(NuError::parse_error(
            format!("Unclosed HTML tag <{}>", close_tag),
            span,
        ))
    }

    fn html_token_text(&mut self) -> NuResult<String> {
        let span = self.current_span();
        match self.peek_kind().clone() {
            TokenKind::Ident(s) | TokenKind::UpperIdent(s) => {
                self.advance();
                Ok(s)
            }
            other if Self::is_keyword_token(&other) => {
                self.advance();
                Ok(other.to_string())
            }
            TokenKind::IntLit(n) => {
                self.advance();
                Ok(n.to_string())
            }
            TokenKind::FloatLit(n) => {
                self.advance();
                Ok(n.to_string())
            }
            TokenKind::StringLit(s) => {
                self.advance();
                Ok(s)
            }
            TokenKind::BoolLit(b) => {
                self.advance();
                Ok(b.to_string())
            }
            TokenKind::NilLit => {
                self.advance();
                Ok("nil".to_string())
            }
            TokenKind::UnitLit => {
                self.advance();
                Ok("unit".to_string())
            }
            other => Err(NuError::parse_error(
                format!("Unexpected token in HTML text: {}", other),
                span,
            )),
        }
    }

    // === Helper Methods ===

    fn is_at_end(&self) -> bool {
        self.peek_kind() == &TokenKind::Eof
    }

    /// Skip tokens until a synchronization point: semicolon, newline followed
    /// by a declaration keyword, or any declaration-start token.
    fn _synchronize(&mut self) {
        // Declaration-start keywords
        const SYNC_TOKENS: &[TokenKind] = &[
            TokenKind::Fn,
            TokenKind::Actor,
            TokenKind::Persistent,
            TokenKind::Entity,
            TokenKind::Organization,
            TokenKind::Virtual,
            TokenKind::StateMachine,
            TokenKind::Agent,
            TokenKind::Workflow,
            TokenKind::Database,
            TokenKind::Type,
            TokenKind::Effect,
            TokenKind::Extern,
            TokenKind::Import,
            TokenKind::Module,
            TokenKind::Pub,
        ];
        while !self.is_at_end() {
            let kind = self.peek_kind().clone();
            if kind == TokenKind::Semicolon || kind == TokenKind::Newline {
                // At a statement boundary: peek ahead past newlines for a
                // declaration keyword. If the next non-newline token is a
                // declaration keyword, stop here.
                let saved = self.pos;
                self.skip_newlines();
                if self.is_at_end() {
                    self.pos = saved;
                    return;
                }
                let next = self.peek_kind();
                if SYNC_TOKENS.contains(next) || *next == TokenKind::Eof {
                    self.pos = saved;
                    return;
                }
                self.pos = saved;
            }
            if SYNC_TOKENS.contains(&kind) {
                return;
            }
            // Track brace depth to avoid syncing inside nested blocks
            self.advance();
        }
    }

    fn peek_kind(&self) -> &TokenKind {
        if self.pos < self.tokens.len() {
            &self.tokens[self.pos].kind
        } else {
            &TokenKind::Eof
        }
    }

    /// Return the current token's kind and advance. The hot path — clones only
    /// the `TokenKind` enum, not the full `Token` with `Span`.
    fn advance(&mut self) -> TokenKind {
        if self.pos < self.tokens.len() {
            let kind = self.tokens[self.pos].kind.clone();
            self.pos += 1;
            kind
        } else {
            TokenKind::Eof
        }
    }

    /// Return the full current token (with span) and advance. Only used where
    /// the caller needs the span (e.g. error messages).
    fn advance_token(&mut self) -> Token {
        if self.pos < self.tokens.len() {
            let tok = self.tokens[self.pos].clone();
            self.pos += 1;
            tok
        } else {
            Token {
                kind: TokenKind::Eof,
                span: self.current_span(),
            }
        }
    }

    fn consume_if(&mut self, kind: &TokenKind) -> bool {
        if self.peek_kind() == kind {
            self.advance();
            true
        } else {
            false
        }
    }

    fn match_token(&self, kind: &TokenKind) -> bool {
        self.peek_kind() == kind
    }

    fn expect(&mut self, kind: TokenKind) -> NuResult<Token> {
        let current_kind = self.peek_kind();
        if current_kind == &kind {
            Ok(self.advance_token())
        } else {
            Err(NuError::parse_error(
                format!("Expected {}", kind),
                self.current_span(),
            ))
        }
    }

    fn expect_ident(&mut self, msg: &str) -> NuResult<String> {
        let current_kind = self.peek_kind();
        match current_kind {
            TokenKind::Ident(s) | TokenKind::UpperIdent(s) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            // `ask` is a reserved keyword that doubles as a valid behavior
            // name (agent actors expose `ask(prompt)` behaviors).
            TokenKind::Ask => {
                self.advance();
                Ok("ask".to_string())
            }
            _ => Err(NuError::parse_error(
                format!("Expected {}, found {}", msg, current_kind),
                self.current_span(),
            )),
        }
    }

    fn expect_string(&mut self, msg: &str) -> NuResult<String> {
        let current_kind = self.peek_kind();
        match current_kind {
            TokenKind::StringLit(s) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            _ => Err(NuError::parse_error(
                format!("Expected {}, found {}", msg, current_kind),
                self.current_span(),
            )),
        }
    }

    fn expect_int(&mut self, msg: &str) -> NuResult<i64> {
        let current_kind = self.peek_kind();
        match current_kind {
            TokenKind::IntLit(n) => {
                let n = *n;
                self.advance();
                Ok(n)
            }
            _ => Err(NuError::parse_error(
                format!("Expected integer {}, found {}", msg, current_kind),
                self.current_span(),
            )),
        }
    }

    fn expect_float(&mut self, msg: &str) -> NuResult<f64> {
        let current_kind = self.peek_kind();
        match current_kind {
            TokenKind::FloatLit(f) => {
                let f = *f;
                self.advance();
                Ok(f)
            }
            _ => Err(NuError::parse_error(
                format!("Expected float {}, found {}", msg, current_kind),
                self.current_span(),
            )),
        }
    }

    fn current_span(&self) -> Span {
        if self.pos < self.tokens.len() {
            self.tokens[self.pos].span
        } else if !self.tokens.is_empty() {
            self.tokens[self.tokens.len() - 1].span
        } else {
            Span::default()
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(
            self.peek_kind(),
            &TokenKind::Newline | &TokenKind::DocComment(_)
        ) {
            self.advance();
        }
    }

    fn skip_newlines_semicolons(&mut self) {
        while matches!(
            self.peek_kind(),
            &TokenKind::Newline | &TokenKind::Semicolon | &TokenKind::DocComment(_)
        ) {
            self.advance();
        }
    }

    fn is_expr_start(&self) -> bool {
        matches!(
            self.peek_kind(),
            TokenKind::IntLit(_)
                | TokenKind::FloatLit(_)
                | TokenKind::StringLit(_)
                | TokenKind::FStringLit(_)
                | TokenKind::BoolLit(_)
                | TokenKind::NilLit
                | TokenKind::UnitLit
                | TokenKind::Ident(_)
                | TokenKind::UpperIdent(_)
                | TokenKind::LParen
                | TokenKind::LBracket
                | TokenKind::LBrace
                | TokenKind::Fn
                | TokenKind::Let
                | TokenKind::If
                | TokenKind::Match
                | TokenKind::Spawn
                | TokenKind::Send
                | TokenKind::Ask
                | TokenKind::Perform
                | TokenKind::Emit
                | TokenKind::Handle
                | TokenKind::Consume
                | TokenKind::Recover
                | TokenKind::Defer
                | TokenKind::Errdefer
                | TokenKind::Receive
                | TokenKind::For
                | TokenKind::While
                | TokenKind::Until
                | TokenKind::Migrate
                | TokenKind::Fail
                | TokenKind::Return
                | TokenKind::Break
                | TokenKind::SelfKw
                | TokenKind::Par
                | TokenKind::Minus
                | TokenKind::Not
                | TokenKind::Bang
                | TokenKind::Ampersand
                | TokenKind::Star
                | TokenKind::True
                | TokenKind::False
                | TokenKind::Unit
                | TokenKind::Lt
        )
    }

    fn is_record_literal_ahead(&self) -> bool {
        if self.peek_kind() == &TokenKind::LBrace {
            if self.pos + 2 < self.tokens.len() {
                let next1 = &self.tokens[self.pos + 1].kind;
                let next2 = &self.tokens[self.pos + 2].kind;
                return matches!(next1, TokenKind::Ident(_) | TokenKind::UpperIdent(_))
                    && matches!(next2, TokenKind::Colon);
            }
        }
        false
    }

    fn parse_array(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.expect(TokenKind::LBracket)?;
        let mut exprs = Vec::new();
        self.skip_newlines();
        while self.peek_kind() != &TokenKind::RBracket && !self.is_at_end() {
            exprs.push(self.parse_expr()?);
            self.skip_newlines();
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        self.expect(TokenKind::RBracket)?;
        Ok(Expr::Array(exprs, span))
    }

    fn parse_record_literal(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.expect(TokenKind::LBrace)?;
        let mut fields = Vec::new();
        self.skip_newlines();
        while self.peek_kind() != &TokenKind::RBrace && !self.is_at_end() {
            let field = self.expect_ident("field name")?;
            self.expect(TokenKind::Colon)?;
            let val = self.parse_expr()?;
            fields.push((field, val));
            self.skip_newlines();
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        self.expect(TokenKind::RBrace)?;
        Ok(Expr::Record(fields, span))
    }

    /// Parse a record-update expression: `{ base .. field = val, ... }`.
    /// `base` has already been parsed and `..` has been consumed by the caller.
    #[allow(dead_code)]
    fn parse_record_update(&mut self, base: Expr) -> NuResult<Expr> {
        let span = self.current_span();
        let mut fields = Vec::new();
        self.skip_newlines();
        while self.peek_kind() != &TokenKind::RBrace && !self.is_at_end() {
            let field = self.expect_ident("field name")?;
            self.expect(TokenKind::Assign)?;
            let val = self.parse_expr()?;
            fields.push((field, val));
            self.skip_newlines();
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        self.expect(TokenKind::RBrace)?;
        Ok(Expr::RecordUpdate {
            base: Box::new(base),
            fields,
            span,
        })
    }
    fn parse_self_ref(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.expect(TokenKind::SelfKw)?;
        Ok(Expr::SelfRef(span))
    }

    fn parse_handle(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.expect(TokenKind::Handle)?;
        let body = self.parse_expr()?;
        let mut handlers = Vec::new();
        if self.consume_if(&TokenKind::With) {
            if self.peek_kind() == &TokenKind::LBrace {
                // inline handlers below
            } else if let TokenKind::Ident(_) = self.peek_kind() {
                let handler_name = self.expect_ident("handler name")?;
                let resolved = self.handler_registry.get(&handler_name).cloned();
                match resolved {
                    Some(h) => handlers.extend(h),
                    None => return Err(NuError::parse_error(format!("undefined handler '{}' -- handler declarations must appear before their use", handler_name), self.current_span())),
                }
                if self.peek_kind() != &TokenKind::LBrace {
                    return Ok(Expr::Handle {
                        body: Box::new(body),
                        handlers,
                        span,
                    });
                }
            } else {
                return Err(NuError::parse_error(
                    "expected '{' or handler name after 'with'".to_string(),
                    self.current_span(),
                ));
            }
        }
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();
        while self.peek_kind() != &TokenKind::RBrace && !self.is_at_end() {
            self.consume_if(&TokenKind::Pipe);
            let effect_name = self.expect_ident("effect name")?;
            self.expect(TokenKind::Dot)?;
            let op_name = self.expect_ident("operation name")?;
            self.expect(TokenKind::LParen)?;
            let mut params = Vec::new();
            self.skip_newlines();
            while self.peek_kind() != &TokenKind::RParen && !self.is_at_end() {
                params.push(self.expect_ident("param name")?);
                self.skip_newlines();
                if !self.consume_if(&TokenKind::Comma) {
                    break;
                }
                self.skip_newlines();
            }
            self.expect(TokenKind::RParen)?;
            let has_resume = self.consume_if(&TokenKind::Resume);
            self.expect(TokenKind::FatArrow)?;
            let handler_body = self.parse_expr()?;
            handlers.push(EffectHandler {
                effect_name,
                op_name,
                params,
                body: handler_body,
                resume: has_resume,
            });
            self.skip_newlines_semicolons();
        }
        self.expect(TokenKind::RBrace)?;
        Ok(Expr::Handle {
            body: Box::new(body),
            handlers,
            span,
        })
    }

    /// Prefix catch: `catch expr fallback` or `catch expr { | pat => body, ... }`
    /// Desugars identically to postfix: `expr catch fallback`
    fn parse_catch_prefix(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance(); // consume 'catch'
        self.warnings.push(NuWarning::deprecated_catch(span));
        let expr = self.parse_expr()?;
        if self.consume_if(&TokenKind::LBrace) {
            // Block form: catch expr { | pat => body, ... }
            let mut arms = Vec::new();
            let x = "__catch_x".to_string();
            // Ok arm: unwrap the success value
            arms.push((
                Pattern::Variant("Ok".to_string(), Some(Box::new(Pattern::Var(x.clone())))),
                None,
                Expr::Var(x, span),
            ));
            // Parse user-provided Error arms
            self.skip_newlines();
            while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
                self.skip_newlines();
                if self.match_token(&TokenKind::RBrace) {
                    break;
                }
                self.consume_if(&TokenKind::Pipe);
                self.skip_newlines();
                let pat = self.parse_pattern()?;
                let guard = if self.consume_if(&TokenKind::If) {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                self.expect(TokenKind::FatArrow)?;
                let body = self.parse_expr()?;
                arms.push((pat, guard, body));
                self.skip_newlines_semicolons();
                self.consume_if(&TokenKind::Comma);
            }
            self.expect(TokenKind::RBrace)?;
            Ok(Expr::Match {
                scrutinee: Box::new(expr),
                arms,
                span,
            })
        } else {
            // Bare form: catch expr fallback_expr
            let fallback = self.parse_expr()?;
            let x = "__catch_x".to_string();
            Ok(Expr::Match {
                scrutinee: Box::new(expr),
                arms: vec![
                    (
                        Pattern::Variant("Ok".to_string(), Some(Box::new(Pattern::Var(x.clone())))),
                        None,
                        Expr::Var(x, span),
                    ),
                    (
                        Pattern::Variant("Error".to_string(), Some(Box::new(Pattern::Wild))),
                        None,
                        fallback,
                    ),
                ],
                span,
            })
        }
    }

    fn parse_receive(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.expect(TokenKind::Receive)?;
        self.expect(TokenKind::LBrace)?;
        let mut arms = Vec::new();
        self.skip_newlines();
        while self.peek_kind() != &TokenKind::RBrace && !self.is_at_end() {
            self.consume_if(&TokenKind::Pipe);
            let behavior_name = self.expect_ident("behavior name")?;
            self.expect(TokenKind::LParen)?;
            let mut params = Vec::new();
            self.skip_newlines();
            while self.peek_kind() != &TokenKind::RParen && !self.is_at_end() {
                params.push(self.parse_pattern()?);
                self.skip_newlines();
                if !self.consume_if(&TokenKind::Comma) {
                    break;
                }
                self.skip_newlines();
            }
            self.expect(TokenKind::RParen)?;
            // Optional guard: `| Behavior(pat) if cond => body`. The guard
            // may reference variables bound by the payload patterns.
            let guard = if self.consume_if(&TokenKind::If) {
                Some(Box::new(self.parse_expr()?))
            } else {
                None
            };
            self.expect(TokenKind::FatArrow)?;
            let body = self.parse_expr()?;
            arms.push((behavior_name, params, guard, body));
            self.skip_newlines_semicolons();
        }
        self.expect(TokenKind::RBrace)?;
        // Optional timeout clause: `receive { ... } after ms_expr => body`.
        // `after` is a contextual keyword — an ordinary identifier expected
        // only in this position (same pattern as `to` in parse_migrate), so
        // user code may still name bindings and workflow steps `after`.
        // Without the clause the receive keeps its non-blocking fallthrough;
        // with it, a no-match suspends up to `ms_expr` milliseconds before
        // running `body` (see mir::RValue::ReceiveWait / OpCode::ReceiveWait).
        let after = if matches!(self.peek_kind(), TokenKind::Ident(s) if s == "after") {
            self.advance(); // consume 'after'
            let timeout_ms = self.parse_expr()?;
            self.expect(TokenKind::FatArrow)?;
            let timeout_body = self.parse_expr()?;
            Some((Box::new(timeout_ms), Box::new(timeout_body)))
        } else {
            None
        };
        Ok(Expr::Receive { arms, after, span })
    }

    fn parse_for(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.expect(TokenKind::For)?;
        let var = self.expect_ident("loop variable")?;
        self.expect(TokenKind::In)?;
        let iterable = self.parse_expr()?;
        let body = self.parse_expr()?;
        Ok(Expr::For {
            var,
            iterable: Box::new(iterable),
            body: Box::new(body),
            span,
        })
    }

    fn parse_while(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.expect(TokenKind::While)?;

        // `while let <pattern> = <scrutinee> { <body> }`
        // Desugars to `while true { match <scrutinee> { <pattern> => <body>, _ => break } }`
        if self.consume_if(&TokenKind::Let) {
            let pat = self.parse_pattern()?;
            self.expect(TokenKind::Assign)?;
            let scrutinee = self.parse_expr()?;
            let body = self.parse_expr()?;
            let match_expr = Expr::Match {
                scrutinee: Box::new(scrutinee),
                arms: vec![
                    (pat, None, body),
                    (Pattern::Wild, None, Expr::Break(None, span)),
                ],
                span,
            };
            return Ok(Expr::While {
                cond: Box::new(Expr::Literal(Literal::Bool(true), span)),
                body: Box::new(match_expr),
                span,
            });
        }

        let cond = self.parse_expr()?;
        let body = self.parse_expr()?;
        Ok(Expr::While {
            cond: Box::new(cond),
            body: Box::new(body),
            span,
        })
    }

    /// Parse `until <condition> [poll <interval>] => <body>`.
    /// Desugars to a polling loop with `Timer.sleep`.
    fn parse_until(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.expect(TokenKind::Until)?;
        let condition = self.parse_expr()?;
        // Optional `poll <interval>` clause
        let poll_ms = if matches!(self.peek_kind(), TokenKind::Ident(s) if s == "poll") {
            self.advance(); // consume 'poll'
            self.parse_expr()?
        } else {
            Expr::Literal(Literal::Int(100), span)
        };
        self.expect(TokenKind::FatArrow)?;
        let body = self.parse_expr()?;

        // Desugar:
        //   let __until_poll = <poll_ms> in
        //   let rec __until_loop = fn() {
        //       if <condition> then <body>
        //       else { perform Timer.sleep(__until_poll); __until_loop() }
        //   } in __until_loop()
        let poll_var = "__until_poll".to_string();
        let loop_fn = "__until_loop".to_string();
        let sleep_expr = Expr::Perform {
            effect: "Timer".to_string(),
            op: "sleep".to_string(),
            args: vec![Expr::Var(poll_var.clone(), span)],
            span,
        };
        let recurse_expr = Expr::App {
            func: Box::new(Expr::Var(loop_fn.clone(), span)),
            args: vec![],
            span,
        };
        let else_block = Expr::Block {
            exprs: vec![sleep_expr, recurse_expr],
            span,
        };
        let if_expr = Expr::If {
            cond: Box::new(condition),
            then_branch: Box::new(body),
            else_branch: Some(Box::new(else_block)),
            span,
        };
        let loop_body = Expr::Let {
            name: loop_fn.clone(),
            ty: None,
            value: Box::new(Expr::Lambda {
                params: vec![],
                ret_type: None,
                body: Box::new(if_expr),
                effect: None,
                span,
            }),
            body: Box::new(Expr::App {
                func: Box::new(Expr::Var(loop_fn, span)),
                args: vec![],
                span,
            }),
            mutable: false,
            span,
            let_in: false,
        };
        Ok(Expr::Let {
            name: poll_var,
            ty: None,
            value: Box::new(poll_ms),
            body: Box::new(loop_body),
            mutable: false,
            span,
            let_in: false,
        })
    }

    fn parse_migrate(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.expect(TokenKind::Migrate)?;
        let actor = self.parse_expr()?;
        let to_ident = self.expect_ident("to")?;
        if to_ident != "to" {
            return Err(NuError::parse_error(
                format!("Expected 'to', found '{}'", to_ident),
                self.current_span(),
            ));
        }
        let node = self.parse_expr()?;
        Ok(Expr::Migrate {
            actor: Box::new(actor),
            node: Box::new(node),
            span,
        })
    }

    fn parse_with_block(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance(); // consume 'with'

        // `with { pat <- expr, pat2 <- expr2, ... } { body }` — error chaining.
        // Desugars to nested `let pat = catch expr => { |e| fail e } in ...`.
        if self.peek_kind() == &TokenKind::LBrace {
            self.advance(); // consume '{'
            let mut bindings: Vec<(Pattern, Expr)> = Vec::new();
            self.skip_newlines();
            while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
                self.skip_newlines();
                let pat = self.parse_pattern()?;
                self.expect(TokenKind::ThinArrow)?; // <-
                let expr = self.parse_expr()?;
                bindings.push((pat, expr));
                self.skip_newlines();
                self.consume_if(&TokenKind::Comma);
                self.skip_newlines();
            }
            self.expect(TokenKind::RBrace)?; // close binding block
            let body = self.parse_block()?; // { body }
                                            // Desugar: chain of let-catch
            let mut result = body;
            for (pat, expr) in bindings.into_iter().rev() {
                let err_var = format!("__we{}", span.start);
                result = Expr::Let {
                    name: match &pat {
                        Pattern::Var(name) => name.clone(),
                        _ => format!("__wp{}", span.start),
                    },
                    ty: None,
                    value: Box::new(Expr::Match {
                        scrutinee: Box::new(expr),
                        arms: vec![
                            (
                                Pattern::Variant("Ok".to_string(), Some(Box::new(pat))),
                                None,
                                result.clone(),
                            ),
                            (
                                Pattern::Variant(
                                    "Error".to_string(),
                                    Some(Box::new(Pattern::Var(err_var.clone()))),
                                ),
                                None,
                                Expr::Return(Some(Box::new(Expr::Var(err_var, span))), span),
                            ),
                        ],
                        span,
                    }),
                    mutable: false,
                    let_in: false,
                    body: Box::new(result),
                    span,
                };
            }
            return Ok(result);
        }

        // Try parsing as `with <expr> as <name> { <body> }` (resource management).
        let saved = self.pos;
        let first = self.parse_expr();

        // Check for `as` keyword
        if self.peek_kind() == &TokenKind::Ident("as".to_string()) {
            match first {
                Ok(resource) => {
                    self.advance(); // consume 'as'
                    let var_name = self.expect_ident("variable name")?;
                    let body = self.parse_block()?;
                    return Ok(Expr::Let {
                        name: var_name,
                        ty: None,
                        value: Box::new(resource),
                        body: Box::new(body),
                        mutable: false,
                        let_in: false,
                        span,
                    });
                }
                Err(_) => {
                    self.pos = saved;
                }
            }
        }

        // Not resource-with — must be handler-with: `with <handler_name> <expr>`
        self.pos = saved;
        let handler_name = self.expect_ident("handler name")?;
        let body = self.parse_expr()?;
        let resolved = self.handler_registry.get(&handler_name).cloned();
        match resolved {
            Some(handlers) => Ok(Expr::Handle {
                body: Box::new(body),
                handlers,
                span,
            }),
            None => Err(NuError::parse_error(
                format!(
                    "undefined handler '{}' -- handler declarations must appear before their use",
                    handler_name
                ),
                self.current_span(),
            )),
        }
    }

    fn parse_consume_expr(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance();
        let expr = self.parse_expr()?;
        Ok(Expr::Consume {
            expr: Box::new(expr),
            span,
        })
    }

    fn parse_recover_expr(&mut self) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance(); // consume 'recover'
        let body = self.parse_block()?;
        Ok(Expr::Recover {
            body: Box::new(body),
            span,
        })
    }

    fn parse_defer_expr(&mut self, error_only: bool) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance(); // consume 'defer' or 'errdefer'
        let expr = self.parse_expr()?;
        Ok(Expr::Defer {
            expr: Box::new(expr),
            error_only,
            span,
        })
    }

    /// Parse `hide a, b { body }` or `seal except a, b { body }`.
    fn parse_hide_expr(&mut self, is_seal: bool) -> NuResult<Expr> {
        let span = self.current_span();
        self.advance(); // consume 'hide' | 'seal'
        if is_seal {
            if !self.match_token(&TokenKind::Except) {
                return Err(NuError::parse_error(
                    "expected 'except' after 'seal'".to_string(),
                    self.current_span(),
                ));
            }
            self.advance(); // consume 'except'
        }
        let mut names = Vec::new();
        loop {
            names.push(self.expect_ident("identifier in hide/seal directive")?);
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
        }
        let body = self.parse_block()?;
        if is_seal {
            Ok(Expr::Seal {
                names,
                body: Box::new(body),
                span,
            })
        } else {
            Ok(Expr::Hide {
                names,
                body: Box::new(body),
                span,
            })
        }
    }
    fn parse_type(&mut self) -> NuResult<Type> {
        self.parse_type_arrow()
    }

    fn parse_type_arrow(&mut self) -> NuResult<Type> {
        let left = self.parse_type_atomic()?;
        if self.consume_if(&TokenKind::Arrow) {
            let right = self.parse_type_arrow()?;
            let effect = if self.consume_if(&TokenKind::Bang) {
                self.parse_effect_row()?
            } else {
                EffectRow::empty()
            };
            let cap = if self.consume_if(&TokenKind::Colon) {
                self.parse_capability()?
            } else {
                Capability::Ref
            };
            Ok(Type::Function {
                param: Box::new(left),
                ret: Box::new(right),
                effect,
                cap,
            })
        } else {
            Ok(left)
        }
    }

    fn parse_type_atomic(&mut self) -> NuResult<Type> {
        let current_kind = self.peek_kind();
        match current_kind {
            TokenKind::Ident(s) | TokenKind::UpperIdent(s) => {
                let name = s.clone();
                let name_span = self.current_span();
                self.advance();

                // Optional type arguments (`Option[Int]`). Parsed up front so
                // a declared generic type can have them substituted into its
                // expansion below.
                let args = if self.peek_kind() == &TokenKind::LBracket {
                    self.advance(); // consume '['
                    let mut args = Vec::new();
                    self.skip_newlines();
                    while self.peek_kind() != &TokenKind::RBracket && !self.is_at_end() {
                        args.push(self.parse_type()?);
                        self.skip_newlines();
                        if !self.consume_if(&TokenKind::Comma) {
                            break;
                        }
                        self.skip_newlines();
                    }
                    self.expect(TokenKind::RBracket)?;
                    args
                } else {
                    Vec::new()
                };

                let ty = match name.as_str() {
                    "Int" => Type::Primitive(PrimitiveType::Int),
                    "Float" => Type::Primitive(PrimitiveType::Float),
                    "Bool" => Type::Primitive(PrimitiveType::Bool),
                    "String" => Type::Primitive(PrimitiveType::String),
                    "Nil" => Type::Primitive(PrimitiveType::Nil),
                    "Unit" => Type::Primitive(PrimitiveType::Unit),
                    "Never" => Type::Primitive(PrimitiveType::Never),
                    "Address" => Type::Primitive(PrimitiveType::Address),
                    _ => {
                        if let Some(&tv) = self.local_type_params.get(&name) {
                            Type::Var(tv)
                        } else {
                            // Declared type names (`type` / `type alias`)
                            // expand to their declaration; truly unknown names
                            // are a hard error instead of a silently
                            // unconstrained fresh variable (SPEC2 §3.4.1).
                            return self.resolve_named_type(&name, args, name_span);
                        }
                    }
                };

                if args.is_empty() {
                    Ok(ty)
                } else {
                    Ok(Type::App {
                        constructor: Box::new(ty),
                        args,
                    })
                }
            }
            TokenKind::LParen => {
                self.advance(); // consume '('
                let mut types = Vec::new();
                self.skip_newlines();
                while self.peek_kind() != &TokenKind::RParen && !self.is_at_end() {
                    types.push(self.parse_type()?);
                    self.skip_newlines();
                    if !self.consume_if(&TokenKind::Comma) {
                        break;
                    }
                    self.skip_newlines();
                }
                self.expect(TokenKind::RParen)?;
                if types.len() == 1 {
                    Ok(types[0].clone())
                } else {
                    Ok(Type::Tuple(types))
                }
            }
            TokenKind::LBrace => {
                self.advance(); // consume '{'
                let mut fields = Vec::new();
                self.skip_newlines();
                while self.peek_kind() != &TokenKind::RBrace && !self.is_at_end() {
                    let fname = self.expect_ident("field name")?;
                    self.expect(TokenKind::Colon)?;
                    let fty = self.parse_type()?;
                    fields.push((fname, fty));
                    self.skip_newlines();
                    if !self.consume_if(&TokenKind::Comma) {
                        break;
                    }
                    self.skip_newlines();
                }
                self.expect(TokenKind::RBrace)?;
                Ok(Type::Record(fields))
            }
            TokenKind::Ampersand => {
                self.advance(); // consume '&'
                let cap = self.parse_capability()?;
                let inner = self.parse_type_atomic()?;
                Ok(Type::Reference {
                    cap,
                    inner: Box::new(inner),
                })
            }
            TokenKind::LBracket => {
                self.advance(); // consume '['
                let inner = self.parse_type()?;
                self.expect(TokenKind::RBracket)?;
                Ok(Type::Array(Box::new(inner)))
            }
            _ => Err(NuError::parse_error(
                format!("Expected type, found {}", current_kind),
                self.current_span(),
            )),
        }
    }

    /// Resolve a non-primitive type name against the module's `type` and
    /// `type alias` declarations. The declaration is re-parsed from the token
    /// stream — the parser is a cursor over a fully lexed token vec, so
    /// forward references work — with the use-site arguments substituted for
    /// the declared type parameters. Unknown names are a hard parse error
    /// instead of a silently unconstrained fresh type variable.
    fn resolve_named_type(&mut self, name: &str, args: Vec<Type>, span: Span) -> NuResult<Type> {
        // First check local type declarations.
        if let Some(decl_pos) = self.find_type_decl(name) {
            return self
                .resolve_local_type(name, &args, decl_pos, span)
                .map(|(_, ty)| ty);
        }
        // Then check cached imported types. The cache stores the
        // declaration's type-parameter variables alongside the resolved
        // body, so use-site type arguments can be spliced in exactly like
        // `resolve_local_type` does for local declarations.
        if let Some((param_vars, ty)) = self.imported_type_cache.get(name) {
            return self.apply_imported_type_args(name, param_vars, ty, &args, span);
        }

        // Try to populate the cache from import statements.
        if let Some((param_vars, ty)) = self.try_import_type(name, span)? {
            return self.apply_imported_type_args(name, &param_vars, &ty, &args, span);
        }

        return Err(NuError::parse_error(
            format!("Unknown type name: '{}'", name),
            span,
        ));
    }

    /// Splice use-site type arguments into an imported type's resolved body,
    /// substituting each argument for the declaration's type-parameter
    /// variable. Mirrors `resolve_local_type`'s substitution for local
    /// declarations; with no arguments the body is returned as-is (its
    /// parameters remain free).
    fn apply_imported_type_args(
        &self,
        name: &str,
        param_vars: &[TypeVar],
        ty: &Type,
        args: &[Type],
        span: Span,
    ) -> NuResult<Type> {
        if !args.is_empty() && args.len() != param_vars.len() {
            return Err(NuError::parse_error(
                format!(
                    "Type '{}' expects {} type argument(s), got {}",
                    name,
                    param_vars.len(),
                    args.len()
                ),
                span,
            ));
        }
        let mut body = ty.clone();
        for (tv, arg) in param_vars.iter().zip(args.iter()) {
            body = Self::subst_type_var(&body, *tv, arg);
        }
        Ok(body)
    }

    /// Find the token index of the `type` keyword of the declaration named
    /// `name` (`type Name = ...` or `type alias Name = ...`), if any.
    fn find_type_decl(&self, name: &str) -> Option<usize> {
        for i in 0..self.tokens.len() {
            let kind = &self.tokens[i].kind;
            if *kind != TokenKind::Type && *kind != TokenKind::Opaque {
                continue;
            }
            let mut j = i + 1;
            // Skip newlines and doc comments between keyword and name.
            while matches!(
                self.tokens.get(j).map(|t| &t.kind),
                Some(TokenKind::Newline) | Some(TokenKind::DocComment(_))
            ) {
                j += 1;
            }
            // Skip optional 'alias' keyword.
            if matches!(self.tokens.get(j).map(|t| &t.kind), Some(TokenKind::Alias)) {
                j += 1;
            }
            // For `opaque type Name`, skip 'type' after 'opaque'.
            if *kind == TokenKind::Opaque {
                if matches!(self.tokens.get(j).map(|t| &t.kind), Some(TokenKind::Type)) {
                    j += 1;
                }
            }
            // Check if the name matches.
            match self.tokens.get(j).map(|t| &t.kind) {
                Some(TokenKind::Ident(n)) | Some(TokenKind::UpperIdent(n)) if n == name => {
                    return Some(i);
                }
                _ => {}
            }
        }
        None
    }

    /// Resolve a local type declaration by re-parsing it from the token stream.
    /// Returns the declaration's type-parameter variables (in order) together
    /// with the body after the use-site arguments have been substituted in;
    /// callers that cache the result keep the parameter variables so later
    /// uses can substitute their own arguments.
    fn resolve_local_type(
        &mut self,
        name: &str,
        args: &[Type],
        decl_pos: usize,
        span: Span,
    ) -> NuResult<(Vec<TypeVar>, Type)> {
        let saved_pos = self.pos;
        let saved_locals = self.local_type_params.clone();
        // Guard against (mutually) recursive references.
        let self_tv = *self
            .global_type_constructors
            .entry(name.to_string())
            .or_insert_with(TypeVar::fresh);
        self.local_type_params.insert(name.to_string(), self_tv);

        // Position the cursor just past the keyword(s): 'type' or 'opaque type'.
        let is_opaque = self.tokens[decl_pos].kind == TokenKind::Opaque;
        self.pos = decl_pos + 1; // skip 'type' or 'opaque'
        if is_opaque {
            self.pos += 1; // also skip 'type' after 'opaque'
        }
        self.skip_newlines();
        let decl_result = if self.peek_kind() == &TokenKind::Alias {
            self.parse_type_alias(false, is_opaque)
        } else {
            self.parse_type_decl_variant_or_record(false, vec![])
        };
        let result = decl_result.and_then(|decl| {
            let (type_params, body) = match decl {
                Decl::TypeAlias {
                    type_params,
                    body,
                    opaque,
                    ..
                } => {
                    if opaque {
                        (
                            type_params,
                            Type::Nominal {
                                name: name.to_string(),
                                underlying: Box::new(body),
                            },
                        )
                    } else {
                        (type_params, body)
                    }
                }
                Decl::RecordType {
                    type_params,
                    fields,
                    ..
                } => (type_params, Type::Record(fields)),
                Decl::VariantType {
                    type_params,
                    variants,
                    ..
                } => (type_params, Type::Variant(variants)),
                _ => unreachable!("find_type_decl only matches type declarations"),
            };
            if !args.is_empty() && args.len() != type_params.len() {
                return Err(NuError::parse_error(
                    format!(
                        "Type '{}' expects {} type argument(s), got {}",
                        name,
                        type_params.len(),
                        args.len()
                    ),
                    span,
                ));
            }
            // Snapshot the declared parameters' variables before the local
            // map is restored, then splice the use-site arguments in.
            let param_vars: Vec<Option<TypeVar>> = type_params
                .iter()
                .map(|p| self.local_type_params.get(p).copied())
                .collect();
            let mut body = body;
            for (tv, arg) in param_vars.iter().zip(args.iter()) {
                if let Some(tv) = tv {
                    body = Self::subst_type_var(&body, *tv, arg);
                }
            }
            Ok((param_vars.into_iter().flatten().collect(), body))
        });
        self.pos = saved_pos;
        self.local_type_params = saved_locals;
        result
    }

    /// Try to find a type name in imported modules. Scans import statements
    /// in the current token stream, loads the imported files, and looks for
    /// matching type declarations. On success, caches the result.
    fn try_import_type(
        &mut self,
        name: &str,
        span: Span,
    ) -> NuResult<Option<(Vec<TypeVar>, Type)>> {
        let mut i = 0;
        while i < self.tokens.len() {
            if self.tokens[i].kind != TokenKind::Import {
                i += 1;
                continue;
            }
            // Extract the import path.
            let mut j = i + 1;
            while j < self.tokens.len()
                && matches!(
                    self.tokens[j].kind,
                    TokenKind::Newline | TokenKind::DocComment(_)
                )
            {
                j += 1;
            }
            if j >= self.tokens.len() {
                break;
            }
            let import_path = match &self.tokens[j].kind {
                TokenKind::Ident(s) => {
                    let mut path = s.clone();
                    j += 1;
                    while j < self.tokens.len() && self.tokens[j].kind == TokenKind::DoubleColon {
                        j += 1;
                        if j < self.tokens.len() {
                            if let TokenKind::Ident(seg) = &self.tokens[j].kind {
                                path.push_str("::");
                                path.push_str(seg);
                                j += 1;
                            } else {
                                break;
                            }
                        }
                    }
                    path
                }
                TokenKind::At => {
                    let mut path = "@".to_string();
                    j += 1;
                    if j < self.tokens.len() {
                        if let TokenKind::Ident(seg) = &self.tokens[j].kind {
                            path.push_str(seg);
                            j += 1;
                        } else {
                            i += 1;
                            continue;
                        }
                    } else {
                        i += 1;
                        continue;
                    }
                    while j < self.tokens.len() && self.tokens[j].kind == TokenKind::Slash {
                        j += 1;
                        if j < self.tokens.len() {
                            if let TokenKind::Ident(seg) = &self.tokens[j].kind {
                                path.push('/');
                                path.push_str(seg);
                                j += 1;
                            } else {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    path
                }
                _ => {
                    i += 1;
                    continue;
                }
            };

            // Resolve the file path (stdlib or relative).
            let file_path = self.resolve_import_path(&import_path);
            if let Some(path) = file_path {
                // Load and lex the file.
                if let Ok(source) = std::fs::read_to_string(&path) {
                    if let Ok(imported_tokens) = crate::lexer::Lexer::new(&source).lex() {
                        // Scan the imported tokens for type declarations.
                        let mut k = 0;
                        while k < imported_tokens.len() {
                            let is_opaque = imported_tokens[k].kind == TokenKind::Opaque;
                            if imported_tokens[k].kind != TokenKind::Type && !is_opaque {
                                k += 1;
                                continue;
                            }
                            let mut m = k + 1;
                            while m < imported_tokens.len()
                                && matches!(
                                    imported_tokens[m].kind,
                                    TokenKind::Newline | TokenKind::DocComment(_)
                                )
                            {
                                m += 1;
                            }
                            // For opaque type declarations, skip the 'type' keyword before the name.
                            if is_opaque
                                && m < imported_tokens.len()
                                && imported_tokens[m].kind == TokenKind::Type
                            {
                                m += 1;
                                while m < imported_tokens.len()
                                    && matches!(
                                        imported_tokens[m].kind,
                                        TokenKind::Newline | TokenKind::DocComment(_)
                                    )
                                {
                                    m += 1;
                                }
                            }
                            // Skip optional 'alias' keyword.
                            if m < imported_tokens.len()
                                && imported_tokens[m].kind == TokenKind::Alias
                            {
                                m += 1;
                            }
                            if m < imported_tokens.len() {
                                match &imported_tokens[m].kind {
                                    TokenKind::Ident(n) | TokenKind::UpperIdent(n) => {
                                        if n == name {
                                            // Found it! Parse the type from the imported file.
                                            let mut sub_parser =
                                                Parser::new(imported_tokens.clone());
                                            sub_parser.global_type_constructors =
                                                self.global_type_constructors.clone();
                                            let (param_vars, resolved) = sub_parser
                                                .resolve_local_type(name, &[], k, span)?;
                                            self.imported_type_cache.insert(
                                                name.to_string(),
                                                (param_vars.clone(), resolved.clone()),
                                            );
                                            return Ok(Some((param_vars, resolved)));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            k += 1;
                        }
                    }
                }
            }
            i += 1;
        }
        Ok(None)
    }

    /// Parse an `app` block: `app "name" { route "GET" "/" -> handler }`.
    fn parse_app_decl(&mut self) -> NuResult<ParsedApp> {
        let span = self.current_span();
        self.advance(); // consume 'app'
        let name = self.expect_string("app name")?;
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();
        let mut routes: Vec<(String, String, String)> = Vec::new();
        while !self.match_token(&TokenKind::RBrace) && !self.is_at_end() {
            self.skip_newlines();
            if self.match_token(&TokenKind::RBrace) {
                break;
            }
            // Contextual keyword 'route' inside an app block.
            if let TokenKind::Ident(s) = self.peek_kind() {
                if s == "route" {
                    self.advance();
                    let method = self.expect_string("route method")?;
                    let path = self.expect_string("route path")?;
                    self.expect(TokenKind::Arrow)?;
                    let handler = self.expect_ident("route handler")?;
                    routes.push((method, path, handler));
                    continue;
                }
            }
            return Err(NuError::parse_error(
                "Expected route declaration inside app block".to_string(),
                self.current_span(),
            ));
        }
        self.expect(TokenKind::RBrace)?;
        Ok(ParsedApp {
            _name: name,
            routes,
            span,
        })
    }

    /// Desugar all collected app blocks into a single `web_main` and a `main`
    /// entrypoint. This is parser-level sugar so no separate runtime path is needed.
    fn desugar_app_decls(&self, apps: Vec<ParsedApp>) -> Vec<Decl> {
        if apps.is_empty() {
            return Vec::new();
        }
        // Flatten all app blocks into a single web_main.
        let mut route_exprs: Vec<Expr> = Vec::new();
        for app in &apps {
            for (method, path, handler) in &app.routes {
                route_exprs.push(Expr::Perform {
                    effect: "Web".to_string(),
                    op: "route".to_string(),
                    args: vec![
                        Expr::Literal(Literal::String(method.clone()), app.span),
                        Expr::Literal(Literal::String(path.clone()), app.span),
                        Expr::Var(handler.clone(), app.span),
                    ],
                    span: app.span,
                });
            }
        }
        let span = apps[0].span;
        let serve_static = Expr::Perform {
            effect: "Web".to_string(),
            op: "serve_static".to_string(),
            args: vec![Expr::Literal(Literal::String("public".to_string()), span)],
            span,
        };
        let mut web_main_body = vec![serve_static];
        web_main_body.extend(route_exprs);
        let web_main = Decl::Function {
            name: "web_main".to_string(),
            type_params: vec![],
            type_param_constraints: vec![],
            params: vec![],
            default_values: vec![],
            using_params: vec![],
            ret_type: Some(Type::unit()),
            error_type: None,
            effect: Some(EffectRow::Closed(vec![Effect::Web])),
            cap: None,
            requires: vec![],
            ensures: vec![],
            body: Expr::Block {
                exprs: web_main_body,
                span,
            },
            annotations: vec![],
            public: false,
            span,
        };
        let main_body = Expr::Block {
            exprs: vec![Expr::App {
                func: Box::new(Expr::Var("web_main".to_string(), span)),
                args: vec![],
                span,
            }],
            span,
        };
        let main = Decl::Function {
            name: "main".to_string(),
            type_params: vec![],
            type_param_constraints: vec![],
            params: vec![],
            default_values: vec![],
            using_params: vec![],
            ret_type: Some(Type::unit()),
            error_type: None,
            effect: None,
            cap: None,
            requires: vec![],
            ensures: vec![],
            body: main_body,
            annotations: vec![],
            public: false,
            span,
        };
        vec![web_main, main]
    }

    /// Check that every route parameter declared in an app route pattern is
    /// read by the handler via `perform Web.param("name")`. This is a basic
    /// static check; it does not follow calls into helper functions or imported
    /// handlers.
    fn check_route_params(&self, apps: &[ParsedApp], decls: &[Decl]) -> NuResult<()> {
        for app in apps {
            for (method, path, handler) in &app.routes {
                for param in Self::route_param_names(path) {
                    let found = decls.iter().any(|decl| match decl {
                        Decl::Function { name, body, .. } if name == handler => {
                            Self::expr_uses_param(body, &param)
                        }
                        _ => false,
                    });
                    if !found {
                        return Err(NuError::parse_error(
                            format!(
                                "Route {} {} declares parameter ':{}' but handler '{}' never calls perform Web.param(\"{}\")",
                                method, path, param, handler, param
                            ),
                            app.span,
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn route_param_names(path: &str) -> Vec<String> {
        let path = path.trim_start_matches('/');
        if path.is_empty() {
            return Vec::new();
        }
        path.split('/')
            .filter(|seg| seg.starts_with(':'))
            .map(|seg| seg[1..].to_string())
            .collect()
    }

    fn expr_uses_param(expr: &Expr, param: &str) -> bool {
        match expr {
            Expr::Perform {
                effect, op, args, ..
            } if effect == "Web" && op == "param" && !args.is_empty() => {
                if let Expr::Literal(Literal::String(name), _) = &args[0] {
                    if name == param {
                        return true;
                    }
                }
            }
            _ => {}
        }
        // Recurse into child expressions.
        match expr {
            Expr::Literal(_, _) | Expr::Var(_, _) | Expr::SelfRef(_) => false,
            Expr::Lambda { body, .. } => Self::expr_uses_param(body, param),
            Expr::App { func, args, .. } => {
                Self::expr_uses_param(func, param)
                    || args.iter().any(|e| Self::expr_uses_param(e, param))
            }
            Expr::Let { value, body, .. } => {
                Self::expr_uses_param(value, param) || Self::expr_uses_param(body, param)
            }
            Expr::LetRec { value, body, .. } => {
                Self::expr_uses_param(value, param) || Self::expr_uses_param(body, param)
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                Self::expr_uses_param(cond, param)
                    || Self::expr_uses_param(then_branch, param)
                    || else_branch
                        .as_ref()
                        .map(|e| Self::expr_uses_param(e, param))
                        .unwrap_or(false)
            }
            Expr::Match {
                scrutinee, arms, ..
            } => {
                Self::expr_uses_param(scrutinee, param)
                    || arms.iter().any(|(_, guard, body)| {
                        Self::expr_uses_param(body, param)
                            || guard
                                .as_ref()
                                .map(|g| Self::expr_uses_param(g, param))
                                .unwrap_or(false)
                    })
            }
            Expr::Block { exprs, .. } | Expr::Par { exprs, .. } => {
                exprs.iter().any(|e| Self::expr_uses_param(e, param))
            }
            Expr::Tuple(exprs, _) | Expr::Array(exprs, _) => {
                exprs.iter().any(|e| Self::expr_uses_param(e, param))
            }
            Expr::Record(fields, _) | Expr::RecordUpdate { fields, .. } => {
                fields.iter().any(|(_, e)| Self::expr_uses_param(e, param))
            }
            Expr::FieldAccess { expr, .. } => Self::expr_uses_param(expr, param),
            Expr::Index { arr, idx, .. } => {
                Self::expr_uses_param(arr, param) || Self::expr_uses_param(idx, param)
            }
            Expr::Binary { left, right, .. } => {
                Self::expr_uses_param(left, param) || Self::expr_uses_param(right, param)
            }
            Expr::Unary { expr, .. } => Self::expr_uses_param(expr, param),
            Expr::Assign { target, value, .. } => {
                Self::expr_uses_param(target, param) || Self::expr_uses_param(value, param)
            }
            Expr::Spawn {
                actor_type, init, ..
            } => {
                Self::expr_uses_param(actor_type, param)
                    || init.iter().any(|(_, e)| Self::expr_uses_param(e, param))
            }
            Expr::Send { actor, args, .. } | Expr::Ask { actor, args, .. } => {
                Self::expr_uses_param(actor, param)
                    || args.iter().any(|e| Self::expr_uses_param(e, param))
            }
            Expr::Receive { arms, after, .. } => {
                arms.iter().any(|(_, _, guard, body)| {
                    Self::expr_uses_param(body, param)
                        || guard
                            .as_ref()
                            .map(|g| Self::expr_uses_param(g, param))
                            .unwrap_or(false)
                }) || after
                    .as_ref()
                    .map(|(t, b)| {
                        Self::expr_uses_param(t, param) || Self::expr_uses_param(b, param)
                    })
                    .unwrap_or(false)
            }
            Expr::Emit { args, .. } => args.iter().any(|e| Self::expr_uses_param(e, param)),
            Expr::Perform { args, .. } => args.iter().any(|e| Self::expr_uses_param(e, param)),
            Expr::GrainRef { key, .. } => Self::expr_uses_param(key, param),
            _ => false,
        }
    }

    /// Resolve an import path to a file path. Handles stdlib:: prefix.
    fn resolve_import_path(&self, import_path: &str) -> Option<std::path::PathBuf> {
        if let Some(module) = import_path.strip_prefix("stdlib::") {
            let module_path = module.replace("::", std::path::MAIN_SEPARATOR_STR);
            // Try NULANG_STDLIB env var first.
            if let Ok(dir) = std::env::var("NULANG_STDLIB") {
                return Some(std::path::PathBuf::from(dir).join(format!("{}.nula", module_path)));
            }
            // Try relative to executable.
            if let Ok(exe) = std::env::current_exe() {
                if let Some(exe_dir) = exe.parent() {
                    let candidate = exe_dir.join("stdlib").join(format!("{}.nula", module_path));
                    if candidate.exists() {
                        return Some(candidate);
                    }
                }
            }
            // Development fallback: src/stdlib/ relative to CWD.
            if let Ok(cwd) = std::env::current_dir() {
                let dev_path = cwd
                    .join("src")
                    .join("stdlib")
                    .join(format!("{}.nula", module_path));
                if dev_path.exists() {
                    return Some(dev_path);
                }
            }
            // Last resort.
            return Some(std::path::PathBuf::from(format!(
                "src/stdlib/{}.nula",
                module_path
            )));
        }

        // @nulang/auth or @nulang/auth/session -> resolved via NULANG_MODULE_PATH.
        if let Some(module) = import_path.strip_prefix("@nulang/") {
            let entries = std::env::var("NULANG_MODULE_PATH").unwrap_or_default();
            for entry in entries.split(';') {
                let entry = entry.trim();
                if entry.is_empty() {
                    continue;
                }
                if let Some((name, dir)) = entry.split_once('=') {
                    let name = name.trim();
                    let dir = dir.trim();
                    if name.is_empty() || dir.is_empty() {
                        continue;
                    }
                    let bare_name = name.strip_prefix("@nulang/").unwrap_or(name);
                    if module == bare_name || module.starts_with(&format!("{}/", bare_name)) {
                        let rest = module.strip_prefix(bare_name).unwrap_or(module);
                        let rest = rest.trim_start_matches('/');
                        let subpath = if rest.is_empty() {
                            "lib.nula"
                        } else {
                            &format!("{}.nula", rest.replace('/', std::path::MAIN_SEPARATOR_STR))
                        };
                        return Some(std::path::PathBuf::from(dir).join(subpath));
                    }
                }
            }
        }

        None
    }

    /// Substitute a single type variable with a concrete type throughout
    /// `ty`. Used to splice use-site arguments into an expanded declared
    /// type; mirrors the type checker's `apply_subst` for one mapping.
    fn subst_type_var(ty: &Type, var: TypeVar, arg: &Type) -> Type {
        match ty {
            Type::Var(v) => {
                if *v == var {
                    arg.clone()
                } else {
                    ty.clone()
                }
            }
            Type::Primitive(_) => ty.clone(),
            Type::Tuple(ts) => Type::Tuple(
                ts.iter()
                    .map(|t| Self::subst_type_var(t, var, arg))
                    .collect(),
            ),
            Type::Record(fs) => Type::Record(
                fs.iter()
                    .map(|(n, t)| (n.clone(), Self::subst_type_var(t, var, arg)))
                    .collect(),
            ),
            Type::Variant(vs) => Type::Variant(
                vs.iter()
                    .map(|(n, t)| {
                        (
                            n.clone(),
                            t.as_ref().map(|t| Self::subst_type_var(t, var, arg)),
                        )
                    })
                    .collect(),
            ),
            Type::Array(t) => Type::Array(Box::new(Self::subst_type_var(t, var, arg))),
            Type::Function {
                param,
                ret,
                effect,
                cap,
            } => Type::Function {
                param: Box::new(Self::subst_type_var(param, var, arg)),
                ret: Box::new(Self::subst_type_var(ret, var, arg)),
                effect: effect.clone(),
                cap: *cap,
            },
            Type::Actor { state, behavior } => Type::Actor {
                state: Box::new(Self::subst_type_var(state, var, arg)),
                behavior: Box::new(Self::subst_type_var(behavior, var, arg)),
            },
            Type::App { constructor, args } => Type::App {
                constructor: Box::new(Self::subst_type_var(constructor, var, arg)),
                args: args
                    .iter()
                    .map(|a| Self::subst_type_var(a, var, arg))
                    .collect(),
            },
            Type::Reference { cap, inner } => Type::Reference {
                cap: *cap,
                inner: Box::new(Self::subst_type_var(inner, var, arg)),
            },
            Type::Nominal { name, underlying } => Type::Nominal {
                name: name.clone(),
                underlying: Box::new(Self::subst_type_var(underlying, var, arg)),
            },
            Type::Skolem(_) => ty.clone(),
            Type::Scheme { vars, body } => {
                if vars.contains(&var) {
                    ty.clone()
                } else {
                    Type::Scheme {
                        vars: vars.clone(),
                        body: Box::new(Self::subst_type_var(body, var, arg)),
                    }
                }
            }
        }
    }

    /// Parse a fixed number of simple patterns for multi-clause function arms.
    /// Each pattern is a literal (`0`, `true`, `"hello"`), wildcard (`_`),
    /// variant (`None`, `Some(x)`), or identifier (variable binding).
    fn parse_clause_patterns(&mut self, expected_count: usize) -> NuResult<Vec<Pattern>> {
        let mut pats = Vec::new();
        self.skip_newlines();
        for i in 0..expected_count {
            if i > 0 {
                self.expect(TokenKind::Comma)?;
                self.skip_newlines();
            }
            let pat = self.parse_clause_pattern()?;
            pats.push(pat);
        }
        Ok(pats)
    }

    /// Parse a single simple pattern for a multi-clause function arm parameter.
    fn parse_clause_pattern(&mut self) -> NuResult<Pattern> {
        match self.peek_kind().clone() {
            TokenKind::IntLit(n) => { self.advance(); Ok(Pattern::Lit(Literal::Int(n))) }
            TokenKind::FloatLit(f) => { self.advance(); Ok(Pattern::Lit(Literal::Float(f))) }
            TokenKind::StringLit(s) => { self.advance(); Ok(Pattern::Lit(Literal::String(s))) }
            TokenKind::True => { self.advance(); Ok(Pattern::Lit(Literal::Bool(true))) }
            TokenKind::False => { self.advance(); Ok(Pattern::Lit(Literal::Bool(false))) }
            TokenKind::NilLit => { self.advance(); Ok(Pattern::Variant("None".to_string(), None)) }
            TokenKind::Ident(s) if s == "_" => { self.advance(); Ok(Pattern::Wild) }
            TokenKind::Ident(name) => {
                self.advance();
                Ok(Pattern::Var(name))
            }
            TokenKind::UpperIdent(name) => {
                self.advance();
                if self.consume_if(&TokenKind::LParen) {
                    let inner = self.parse_clause_pattern()?;
                    self.expect(TokenKind::RParen)?;
                    Ok(Pattern::Variant(name, Some(Box::new(inner))))
                } else {
                    Ok(Pattern::Variant(name, None))
                }
            }
            _ => Err(NuError::parse_error(
                format!(
                    "expected a literal, identifier, or variant pattern in function clause, found {:?}",
                    self.peek_kind()
                ),
                self.current_span(),
            )),
        }
    }

    fn parse_capability(&mut self) -> NuResult<Capability> {
        let current_kind = self.peek_kind();
        match current_kind {
            TokenKind::Iso => {
                self.advance();
                Ok(Capability::Iso)
            }
            TokenKind::Trn => {
                self.advance();
                Ok(Capability::Trn)
            }
            TokenKind::Ref => {
                self.advance();
                Ok(Capability::Ref)
            }
            TokenKind::Val => {
                self.advance();
                Ok(Capability::Val)
            }
            TokenKind::Box => {
                self.advance();
                Ok(Capability::Box)
            }
            TokenKind::Tag => {
                self.advance();
                Ok(Capability::Tag)
            }
            TokenKind::LinearIso => {
                self.advance();
                Ok(Capability::LinearIso)
            }
            TokenKind::Linear => {
                self.advance();
                Ok(Capability::Linear)
            }
            _ => Err(NuError::parse_error(format!(
                    "Expected capability (iso, trn, ref, val, box, tag, lineariso, linear), found {}",
                    current_kind
                ), self.current_span())),
        }
    }

    fn parse_effect_row(&mut self) -> NuResult<EffectRow> {
        let mut effects = Vec::new();
        if self.consume_if(&TokenKind::LBrace) {
            self.skip_newlines();
            let mut is_open = false;
            let mut region = Region(0);
            while self.peek_kind() != &TokenKind::RBrace && !self.is_at_end() {
                if self.consume_if(&TokenKind::Pipe) {
                    let _rname = self.expect_ident("row variable")?;
                    region = Region::fresh();
                    is_open = true;
                    break;
                }
                let name = self.expect_ident("effect name")?;
                effects.push(self.string_to_effect(&name));
                self.skip_newlines();
                if !self.consume_if(&TokenKind::Comma) {
                    break;
                }
                self.skip_newlines();
            }
            self.expect(TokenKind::RBrace)?;
            if is_open {
                Ok(EffectRow::Open(effects, region))
            } else {
                Ok(EffectRow::Closed(effects))
            }
        } else {
            let name = self.expect_ident("effect name")?;
            Ok(EffectRow::Closed(vec![self.string_to_effect(&name)]))
        }
    }

    fn string_to_effect(&self, name: &str) -> Effect {
        // Single name table shared with the effect checker so annotation
        // parsing and `perform` resolution can never disagree on the
        // built-in effect names (SPEC2 §4.6).
        crate::effect_checker::parse_effect_name(name)
    }

    fn parse_pattern(&mut self) -> NuResult<Pattern> {
        let pat = self.parse_pattern_atomic()?;
        if self.consume_if(&TokenKind::At) {
            if let Pattern::Var(name) = pat {
                let sub = self.parse_pattern()?;
                Ok(Pattern::Alias(name, Box::new(sub)))
            } else {
                Err(NuError::parse_error(
                    "Left side of '@' alias must be a variable".to_string(),
                    self.current_span(),
                ))
            }
        } else {
            Ok(pat)
        }
    }

    fn parse_pattern_atomic(&mut self) -> NuResult<Pattern> {
        let current_kind = self.peek_kind();
        match current_kind {
            TokenKind::Ident(s) if s == "_" => {
                self.advance();
                Ok(Pattern::Wild)
            }
            TokenKind::Ident(name) => {
                let name = name.clone();
                self.advance();
                Ok(Pattern::Var(name))
            }
            TokenKind::UpperIdent(name) => {
                let name = name.clone();
                self.advance();
                if self.consume_if(&TokenKind::LParen) {
                    let sub = self.parse_pattern()?;
                    self.expect(TokenKind::RParen)?;
                    Ok(Pattern::Variant(name, Some(Box::new(sub))))
                } else {
                    Ok(Pattern::Variant(name, None))
                }
            }
            TokenKind::LParen => {
                self.advance();
                let mut pats = Vec::new();
                self.skip_newlines();
                while self.peek_kind() != &TokenKind::RParen && !self.is_at_end() {
                    pats.push(self.parse_pattern()?);
                    self.skip_newlines();
                    if !self.consume_if(&TokenKind::Comma) {
                        break;
                    }
                    self.skip_newlines();
                }
                self.expect(TokenKind::RParen)?;
                if pats.len() == 1 {
                    Ok(pats[0].clone())
                } else {
                    Ok(Pattern::Tuple(pats))
                }
            }
            TokenKind::LBrace => {
                self.advance();
                let mut fields = Vec::new();
                self.skip_newlines();
                while self.peek_kind() != &TokenKind::RBrace && !self.is_at_end() {
                    let fname = self.expect_ident("field name")?;
                    self.expect(TokenKind::Colon)?;
                    let fpat = self.parse_pattern()?;
                    fields.push((fname, fpat));
                    self.skip_newlines();
                    if !self.consume_if(&TokenKind::Comma) {
                        break;
                    }
                    self.skip_newlines();
                }
                self.expect(TokenKind::RBrace)?;
                Ok(Pattern::Record(fields))
            }
            TokenKind::IntLit(v) => {
                let v = *v;
                self.advance();
                Ok(Pattern::Lit(Literal::Int(v)))
            }
            TokenKind::FloatLit(v) => {
                let v = *v;
                self.advance();
                Ok(Pattern::Lit(Literal::Float(v)))
            }
            TokenKind::StringLit(s) => {
                let s = s.clone();
                self.advance();
                Ok(Pattern::Lit(Literal::String(s)))
            }
            TokenKind::BoolLit(b) => {
                let b = *b;
                self.advance();
                Ok(Pattern::Lit(Literal::Bool(b)))
            }
            TokenKind::NilLit => {
                self.advance();
                Ok(Pattern::Lit(Literal::Nil))
            }
            TokenKind::UnitLit => {
                self.advance();
                Ok(Pattern::Lit(Literal::Unit))
            }
            TokenKind::True => {
                self.advance();
                Ok(Pattern::Lit(Literal::Bool(true)))
            }
            TokenKind::False => {
                self.advance();
                Ok(Pattern::Lit(Literal::Bool(false)))
            }
            TokenKind::Unit => {
                self.advance();
                Ok(Pattern::Lit(Literal::Unit))
            }
            _ => Err(NuError::parse_error(
                format!("Expected pattern, found {}", current_kind),
                self.current_span(),
            )),
        }
    }

    fn parse_variants(&mut self) -> NuResult<Vec<(String, Option<Type>)>> {
        let mut variants = Vec::new();
        self.skip_newlines();
        self.consume_if(&TokenKind::Pipe);
        self.skip_newlines();
        while !self.is_at_end() {
            let name = self.expect_ident("variant name")?;
            let ty = if self.consume_if(&TokenKind::LParen) {
                let t = self.parse_type()?;
                self.expect(TokenKind::RParen)?;
                Some(t)
            } else {
                None
            };
            variants.push((name, ty));
            self.skip_newlines();
            if !self.consume_if(&TokenKind::Pipe) {
                break;
            }
            self.skip_newlines();
        }
        Ok(variants)
    }

    fn parse_record_type_fields(&mut self) -> NuResult<Vec<(String, Type)>> {
        let mut fields = Vec::new();
        self.skip_newlines();
        while self.peek_kind() != &TokenKind::RBrace && !self.is_at_end() {
            let fname = self.expect_ident("field name")?;
            self.expect(TokenKind::Colon)?;
            let fty = self.parse_type()?;
            fields.push((fname, fty));
            self.skip_newlines();
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        self.expect(TokenKind::RBrace)?;
        Ok(fields)
    }

    fn parse_arg_list(&mut self) -> NuResult<Vec<Expr>> {
        let mut args = Vec::new();
        self.skip_newlines();
        while self.peek_kind() != &TokenKind::RParen && !self.is_at_end() {
            args.push(self.parse_expr()?);
            self.skip_newlines();
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        self.expect(TokenKind::RParen)?;
        Ok(args)
    }

    fn parse_type_params(&mut self) -> NuResult<Vec<String>> {
        let mut params = Vec::new();
        if self.consume_if(&TokenKind::LBracket) {
            self.skip_newlines();
            while self.peek_kind() != &TokenKind::RBracket && !self.is_at_end() {
                let name = self.expect_ident("type parameter name")?;
                params.push(name.clone());
                let tv = TypeVar::fresh();
                self.local_type_params.insert(name, tv);
                self.skip_newlines();
                if !self.consume_if(&TokenKind::Comma) {
                    break;
                }
                self.skip_newlines();
            }
            self.expect(TokenKind::RBracket)?;
        }
        Ok(params)
    }

    /// Parse type parameters with optional class constraints.
    /// Returns (type_param_names, [(param_name, type_var, [class_names])]).
    /// `[T]` → (["T"], [])
    /// `[T: Ord]` → (["T"], [("T", tv, ["Ord"])])
    /// `[T: Eq + Ord]` → (["T"], [("T", tv, ["Eq", "Ord"])])
    fn parse_type_params_with_constraints(
        &mut self,
    ) -> NuResult<(Vec<String>, Vec<(String, TypeVar, Vec<String>)>)> {
        let mut names = Vec::new();
        let mut constraints: Vec<(String, TypeVar, Vec<String>)> = Vec::new();
        if self.consume_if(&TokenKind::LBracket) {
            self.skip_newlines();
            while self.peek_kind() != &TokenKind::RBracket && !self.is_at_end() {
                let name = self.expect_ident("type parameter name")?;
                names.push(name.clone());
                let tv = TypeVar::fresh();
                self.local_type_params.insert(name.clone(), tv);
                let mut class_names = Vec::new();
                if self.consume_if(&TokenKind::Colon) {
                    // Parse class constraint list: `Ord` or `Eq + Ord`
                    loop {
                        let cn = self.expect_ident("class name after ':'")?;
                        class_names.push(cn);
                        if !self.consume_if(&TokenKind::Plus) {
                            break;
                        }
                    }
                }
                if !class_names.is_empty() {
                    constraints.push((name.clone(), tv, class_names));
                }
                self.skip_newlines();
                if !self.consume_if(&TokenKind::Comma) {
                    break;
                }
                self.skip_newlines();
            }
            self.expect(TokenKind::RBracket)?;
        }
        Ok((names, constraints))
    }

    fn try_parse_param_capability(&mut self) -> NuResult<Option<Capability>> {
        let kind = self.peek_kind().clone();
        let cap = match &kind {
            TokenKind::Iso => Some(Capability::Iso),
            TokenKind::Trn => Some(Capability::Trn),
            TokenKind::Ref => Some(Capability::Ref),
            TokenKind::Val => Some(Capability::Val),
            TokenKind::Box => Some(Capability::Box),
            TokenKind::Tag => Some(Capability::Tag),
            TokenKind::LinearIso => Some(Capability::LinearIso),
            TokenKind::Linear => Some(Capability::Linear),
            _ => None,
        };
        if cap.is_some() {
            self.advance();
            if !matches!(self.peek_kind(), TokenKind::Ident(_)) {
                return Err(NuError::parse_error(
                    "capability annotation must be followed by a parameter name".to_string(),
                    self.current_span(),
                ));
            }
        }
        Ok(cap)
    }

    fn parse_params(&mut self) -> NuResult<Vec<crate::ast::Param>> {
        let mut params = Vec::new();
        self.skip_newlines();
        while self.peek_kind() != &TokenKind::RParen && !self.is_at_end() {
            let cap = self.try_parse_param_capability().unwrap_or(None);
            let name = self.expect_ident("parameter name")?;
            let ty = if self.consume_if(&TokenKind::Colon) {
                Some(self.parse_type()?)
            } else {
                None
            };
            params.push(crate::ast::Param { name, ty, cap });
            self.skip_newlines();
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        Ok(params)
    }

    /// Parse function parameters with optional default values.
    /// Returns params and a parallel vec of default expressions (None = required).
    fn parse_params_with_defaults(
        &mut self,
    ) -> NuResult<(Vec<crate::ast::Param>, Vec<Option<Expr>>)> {
        let mut params = Vec::new();
        let mut defaults = Vec::new();
        self.skip_newlines();
        while self.peek_kind() != &TokenKind::RParen && !self.is_at_end() {
            let cap = self.try_parse_param_capability().unwrap_or(None);
            let name = self.expect_ident("parameter name")?;
            let ty = if self.consume_if(&TokenKind::Colon) {
                Some(self.parse_type()?)
            } else {
                None
            };
            let default = if self.consume_if(&TokenKind::Assign) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            params.push(crate::ast::Param { name, ty, cap });
            defaults.push(default);
            self.skip_newlines();
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        Ok((params, defaults))
    }

    /// Parse method parameters for class/impl methods. Accepts `self` keyword
    /// as a parameter name in addition to regular identifiers.
    fn parse_method_params(&mut self) -> NuResult<Vec<crate::ast::Param>> {
        let mut params = Vec::new();
        self.skip_newlines();
        while self.peek_kind() != &TokenKind::RParen && !self.is_at_end() {
            let cap = self.try_parse_param_capability().unwrap_or(None);
            let name = match self.peek_kind() {
                TokenKind::SelfKw => {
                    self.advance();
                    "self".to_string()
                }
                _ => self.expect_ident("parameter name")?,
            };
            let ty = if self.consume_if(&TokenKind::Colon) {
                Some(self.parse_type()?)
            } else {
                None
            };
            params.push(crate::ast::Param { name, ty, cap });
            self.skip_newlines();
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        Ok(params)
    }
}

fn token_to_binop(kind: &TokenKind) -> Option<BinOp> {
    match kind {
        TokenKind::DotDot => Some(BinOp::Range),
        TokenKind::Plus => Some(BinOp::Add),
        TokenKind::Minus => Some(BinOp::Sub),
        TokenKind::Star2 => Some(BinOp::Pow),
        TokenKind::Star => Some(BinOp::Mul),
        TokenKind::Slash => Some(BinOp::Div),
        TokenKind::Percent => Some(BinOp::Mod),
        TokenKind::Eq => Some(BinOp::Eq),
        TokenKind::Ne => Some(BinOp::Ne),
        TokenKind::Lt => Some(BinOp::Lt),
        TokenKind::Le => Some(BinOp::Le),
        TokenKind::Gt => Some(BinOp::Gt),
        TokenKind::Ge => Some(BinOp::Ge),
        TokenKind::And => Some(BinOp::And),
        TokenKind::Or => Some(BinOp::Or),
        TokenKind::Ampersand => Some(BinOp::BitAnd),
        TokenKind::Pipe => Some(BinOp::BitOr),
        TokenKind::Pipe3 => Some(BinOp::BitOr),
        TokenKind::Caret => Some(BinOp::BitXor),
        TokenKind::Shl => Some(BinOp::Shl),
        TokenKind::Shr => Some(BinOp::Shr),
        TokenKind::Assign => Some(BinOp::Assign),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;

    fn parse(source: &str) -> NuResult<AstModule> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex()?;
        let mut parser = Parser::new(tokens);
        parser.parse_module()
    }

    fn parse_expr(source: &str) -> NuResult<Expr> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex()?;
        let mut parser = Parser::new(tokens);
        parser.parse_expr()
    }

    #[test]
    fn test_parse_record_type() {
        let ast = parse("type Point = { x: Int, y: Int }").unwrap();
        assert_eq!(ast.decls.len(), 1);
        match &ast.decls[0] {
            Decl::RecordType { name, fields, .. } => {
                assert_eq!(name, "Point");
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[0].0, "x");
                assert_eq!(fields[0].1, Type::Primitive(PrimitiveType::Int));
                assert_eq!(fields[1].0, "y");
                assert_eq!(fields[1].1, Type::Primitive(PrimitiveType::Int));
            }
            _ => panic!("Expected record type declaration"),
        }
    }

    #[test]
    fn test_parse_variant_type() {
        let ast = parse("type Option[T] = Some(T) | None").unwrap();
        match &ast.decls[0] {
            Decl::VariantType {
                name,
                type_params,
                variants,
                ..
            } => {
                assert_eq!(name, "Option");
                assert_eq!(type_params, &["T"]);
                assert_eq!(variants.len(), 2);
                assert_eq!(variants[0].0, "Some");
                assert!(variants[0].1.is_some());
                assert_eq!(variants[1].0, "None");
                assert!(variants[1].1.is_none());
            }
            _ => panic!("Expected variant type declaration"),
        }
    }

    #[test]
    fn test_parse_effect_decl() {
        // Parenthesize the argument so the effect-decl parser does not consume
        // the `->` as part of a function-typed argument.
        let ast = parse("effect IO { print: (String) -> Unit }").unwrap();
        match &ast.decls[0] {
            Decl::EffectDecl { name, ops, .. } => {
                assert_eq!(name, "IO");
                assert_eq!(ops.len(), 1);
                assert_eq!(ops[0].0, "print");
                assert_eq!(ops[0].1, vec![Type::Primitive(PrimitiveType::String)]);
                assert_eq!(ops[0].2, Type::Primitive(PrimitiveType::Unit));
            }
            _ => panic!("Expected effect declaration"),
        }
    }

    #[test]
    fn test_parse_effect_row_builtin_event_and_ffi() {
        // SPEC2 §4.6 lists Event and FFI as built-in effects; annotation
        // parsing must map them to the built-in variants (not UserDefined),
        // exactly like `perform` resolution in the effect checker does.
        let ast = parse("fn f() -> Unit ! {Event, FFI} 1").unwrap();
        match &ast.decls[0] {
            Decl::Function {
                effect: Some(row), ..
            } => {
                assert_eq!(
                    row,
                    &EffectRow::Closed(vec![Effect::Event, Effect::FFI]),
                    "Event and FFI must parse as built-in effects"
                );
            }
            _ => panic!("Expected annotated function declaration"),
        }
    }

    #[test]
    fn test_parse_type_alias() {
        let ast = parse("type alias MyInt = Int").unwrap();
        match &ast.decls[0] {
            Decl::TypeAlias { name, body, .. } => {
                assert_eq!(name, "MyInt");
                assert_eq!(body, &Type::Primitive(PrimitiveType::Int));
            }
            _ => panic!("Expected type alias declaration"),
        }
    }

    #[test]
    fn test_parse_nil_primitive_type() {
        // `Nil` (uppercase) must parse as the primitive Nil type, not a
        // silently unconstrained fresh type variable.
        let ast = parse("fn f(x: Nil) x").unwrap();
        match &ast.decls[0] {
            Decl::Function { params, .. } => {
                assert_eq!(params[0].ty, Some(Type::Primitive(PrimitiveType::Nil)));
            }
            _ => panic!("Expected function declaration"),
        }
    }

    #[test]
    fn test_parse_unknown_type_name_errors() {
        let result = parse("fn f(x: Bogus) x");
        match result {
            Err(NuError::ParseError { msg, .. }) => {
                assert!(
                    msg.contains("Unknown type name") && msg.contains("Bogus"),
                    "unexpected message: {}",
                    msg
                );
            }
            other => panic!("expected unknown type name error, got {:?}", other),
        }
    }

    #[test]
    fn test_prelude_types_resolve_in_annotations() {
        // Prelude constructors (Ok/Some) type-check in every module while
        // the prelude's type declarations are prepended only after the
        // user module parses — so annotated uses used to fail with
        // "Unknown type name". Every Parser now seeds the prelude's
        // resolved types into its imported-type cache.
        let ok = parse("fn f(x: Option[Int]) -> Int { 0 }");
        assert!(
            ok.is_ok(),
            "Option[Int] must parse without import: {:?}",
            ok.err()
        );
        let ok = parse("fn f(x: Result[Int, String]) -> Int { 0 }");
        assert!(
            ok.is_ok(),
            "Result[Int, String] must parse without import: {:?}",
            ok.err()
        );
    }

    #[test]
    fn test_local_type_shadows_prelude_in_annotation() {
        // A module-level `type Option[T]` shadows the prelude's — the
        // resolved body must be the LOCAL declaration's.
        let ast = parse("type Option[T] = Some(T) | None\nfn f(x: Option[Int]) -> Int { 0 }")
            .expect("module parses");
        let has_fn = ast.decls.iter().any(|d| matches!(d, Decl::Function { .. }));
        assert!(has_fn, "function declaration present");
    }

    #[test]
    fn test_parse_declared_alias_expands_in_annotation() {
        let ast = parse("type alias MyInt = Int\nfn f(x: MyInt) x").unwrap();
        match &ast.decls[1] {
            Decl::Function { params, .. } => {
                assert_eq!(params[0].ty, Some(Type::Primitive(PrimitiveType::Int)));
            }
            _ => panic!("Expected function declaration"),
        }
    }

    #[test]
    fn test_parse_declared_variant_expands_with_args() {
        // `Option[Int]` expands to the variant structure with `T := Int`.
        let ast = parse("type Option[T] = Some(T) | None\nfn f(x: Option[Int]) x").unwrap();
        match &ast.decls[1] {
            Decl::Function { params, .. } => match &params[0].ty {
                Some(Type::Variant(variants)) => {
                    assert_eq!(variants.len(), 2);
                    assert_eq!(variants[0].0, "Some");
                    assert_eq!(variants[0].1, Some(Type::Primitive(PrimitiveType::Int)));
                    assert_eq!(variants[1].0, "None");
                    assert_eq!(variants[1].1, None);
                }
                other => panic!("expected expanded variant annotation, got {:?}", other),
            },
            _ => panic!("Expected function declaration"),
        }
    }

    #[test]
    fn test_parse_type_argument_arity_error() {
        let result = parse("type Option[T] = Some(T) | None\nfn f(x: Option[Int, String]) x");
        match result {
            Err(NuError::ParseError { msg, .. }) => {
                assert!(msg.contains("type argument"), "unexpected message: {}", msg);
            }
            other => panic!("expected arity error, got {:?}", other),
        }
    }

    #[test]
    fn test_imported_generic_type_expands_with_args() {
        // `Option[Int]` imported from stdlib::option must expand with the
        // use-site argument substituted for the declaration's `T` — both on
        // the first use (cache miss) and on later uses (cache hit with a
        // different argument). The argument must not be silently dropped.
        let ast = parse(
            "import stdlib::option\n\
             fn f(x: Option[Int]) x\n\
             fn g(x: Option[String]) x",
        )
        .unwrap();
        match &ast.decls[1] {
            Decl::Function { params, .. } => match &params[0].ty {
                Some(Type::Variant(variants)) => {
                    assert_eq!(variants.len(), 2);
                    assert_eq!(variants[0].0, "Some");
                    assert_eq!(
                        variants[0].1,
                        Some(Type::Primitive(PrimitiveType::Int)),
                        "first use of imported Option[Int] must substitute Int"
                    );
                    assert_eq!(variants[1].0, "None");
                    assert_eq!(variants[1].1, None);
                }
                other => panic!("expected expanded variant annotation, got {:?}", other),
            },
            _ => panic!("Expected function declaration"),
        }
        match &ast.decls[2] {
            Decl::Function { params, .. } => match &params[0].ty {
                Some(Type::Variant(variants)) => {
                    assert_eq!(
                        variants[0].1,
                        Some(Type::Primitive(PrimitiveType::String)),
                        "cached imported Option must substitute the new argument"
                    );
                }
                other => panic!("expected expanded variant annotation, got {:?}", other),
            },
            _ => panic!("Expected function declaration"),
        }
    }

    #[test]
    fn test_imported_generic_type_argument_arity_error() {
        let result = parse("import stdlib::option\nfn f(x: Option[Int, String]) x");
        match result {
            Err(NuError::ParseError { msg, .. }) => {
                assert!(
                    msg.contains("expects 1 type argument(s), got 2"),
                    "unexpected message: {}",
                    msg
                );
            }
            other => panic!("expected arity error, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_module_decl() {
        let ast = parse("module M { fn f() 1 }").unwrap();
        match &ast.decls[0] {
            Decl::Module { name, decls, .. } => {
                assert_eq!(name, "M");
                assert_eq!(decls.len(), 1);
                assert!(matches!(&decls[0], Decl::Function { name, .. } if name == "f"));
            }
            _ => panic!("Expected module declaration"),
        }
    }

    #[test]
    fn test_parse_import() {
        let ast = parse("import Foo").unwrap();
        match &ast.decls[0] {
            Decl::Import { path, .. } => {
                assert_eq!(path, "Foo");
            }
            _ => panic!("Expected import declaration"),
        }
    }

    #[test]
    fn test_parse_entity_decl() {
        let source = r#"entity Counter {
            state count = 0
            state local cache: Int = 0
            behavior get() { self.count }
        }"#;
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex().unwrap();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module().unwrap();
        assert_eq!(ast.decls.len(), 1);
        match &ast.decls[0] {
            Decl::Actor {
                name,
                persistent,
                state_fields,
                ..
            } => {
                assert_eq!(name, "Counter");
                assert!(*persistent, "entity should be persistent by default");
                assert_eq!(state_fields.len(), 2);
                assert_eq!(state_fields[0].0, "count");
                assert_eq!(
                    state_fields[0].1,
                    StateModel::EventSourced,
                    "entity state defaults to event_sourced"
                );
                assert_eq!(state_fields[1].0, "cache");
                assert_eq!(
                    state_fields[1].1,
                    StateModel::Local,
                    "explicit local annotation overrides default"
                );
            }
            _ => panic!("Expected Actor decl from entity desugaring"),
        }
    }

    #[test]
    fn test_parse_organization_decl() {
        let source = r#"organization Team {
            state lead: Int = 0
            behavior get() { self.lead }
        }"#;
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex().unwrap();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module().unwrap();
        assert_eq!(ast.decls.len(), 1);
        match &ast.decls[0] {
            Decl::Actor {
                name,
                persistent,
                state_fields,
                ..
            } => {
                assert_eq!(name, "Team");
                assert!(*persistent, "organization should be persistent by default");
                assert_eq!(state_fields.len(), 1);
                assert_eq!(state_fields[0].0, "lead");
                assert_eq!(
                    state_fields[0].1,
                    StateModel::EventSourced,
                    "organization state defaults to event_sourced"
                );
            }
            _ => panic!("Expected Actor decl from organization desugaring"),
        }
    }
    #[test]
    fn test_parse_entity_with_events_block() {
        let source = r#"entity BankAccount {
            state balance: Int = 0
            events
                | Deposited(amount: Int)
                | Withdrawn(amount: Int)
            behavior deposit(amount: Int) { self.balance = self.balance + amount }
        }"#;
        let ast = parse(source).unwrap();
        match &ast.decls[0] {
            Decl::Actor { name, events, .. } => {
                assert_eq!(name, "BankAccount");
                assert_eq!(events.len(), 2);
                assert_eq!(events[0].name, "Deposited");
                assert_eq!(events[0].params.len(), 1);
                assert_eq!(events[0].params[0].0, "amount");
                assert!(matches!(
                    events[0].params[0].1,
                    Type::Primitive(PrimitiveType::Int)
                ));
                assert_eq!(events[1].name, "Withdrawn");
                assert_eq!(events[1].params.len(), 1);
                assert_eq!(events[1].params[0].0, "amount");
            }
            _ => panic!("Expected Actor decl"),
        }
    }

    #[test]
    fn test_parse_entity_with_apply_block() {
        let source = r#"entity Counter {
            state count: Int = 0
            events
                | Incremented(by: Int)
            apply
                | Incremented(by) => self.count = self.count + by
            behavior inc(by: Int) { self.count = self.count + by }
        }"#;
        let ast = parse(source).unwrap();
        match &ast.decls[0] {
            Decl::Actor {
                name,
                ref events,
                ref apply_handlers,
                ..
            } => {
                assert_eq!(name, "Counter");
                assert_eq!(events.len(), 1);
                assert_eq!(apply_handlers.len(), 1);
                assert_eq!(apply_handlers[0].event, "Incremented");
                assert_eq!(apply_handlers[0].params, vec!["by"]);
            }
            _ => panic!("Expected Actor decl"),
        }
    }

    #[test]
    fn test_parse_entity_events_without_params() {
        let source = r#"entity Ticker {
            state last: Int = 0
            events
                | Tick
                | Tock
            behavior tick() { self.last = 1 }
        }"#;
        let ast = parse(source).unwrap();
        match &ast.decls[0] {
            Decl::Actor { events, .. } => {
                assert_eq!(events.len(), 2);
                assert_eq!(events[0].name, "Tick");
                assert!(events[0].params.is_empty());
                assert_eq!(events[1].name, "Tock");
                assert!(events[1].params.is_empty());
            }
            _ => panic!("Expected Actor decl"),
        }
    }

    #[test]
    fn test_parse_actor_decl() {
        let source = r#"actor Counter {
            state count = 0
            behavior get() { self.count }
        }"#;
        let ast = parse(source).unwrap();
        match &ast.decls[0] {
            Decl::Actor {
                name,
                persistent,
                state_fields,
                behaviors,
                ..
            } => {
                assert_eq!(name, "Counter");
                assert!(!persistent);
                assert_eq!(state_fields.len(), 1);
                assert_eq!(state_fields[0].0, "count");
                assert_eq!(state_fields[0].1, StateModel::Local);
                assert_eq!(behaviors.len(), 1);
                assert_eq!(behaviors[0].name, "get");
            }
            _ => panic!("Expected actor declaration"),
        }
    }

    #[test]
    fn test_parse_virtual_entity() {
        let source = r#"virtual entity User(key: String) {
            state durable name: String = ""
            behavior Greet(who: String) { perform IO.print("hi") }
        }"#;
        let ast = parse(source).unwrap();
        match &ast.decls[0] {
            Decl::Actor {
                name,
                persistent,
                virtual_,
                key_params,
                ..
            } => {
                assert_eq!(name, "User");
                assert!(*persistent, "virtual entity should be persistent");
                assert!(*virtual_, "entity should be marked virtual");
                assert_eq!(key_params.len(), 1);
                assert_eq!(key_params[0].name, "key");
                assert_eq!(key_params[0].ty, Some(Type::string()));
            }
            _ => panic!("Expected virtual entity declaration"),
        }
    }

    #[test]
    fn test_parse_grain_ref() {
        let expr = parse_expr(r#"Grain("User", "u1")"#).unwrap();
        match expr {
            Expr::GrainRef {
                grain_type, key, ..
            } => {
                assert_eq!(grain_type, "User");
                match key.as_ref() {
                    Expr::Literal(Literal::String(s), _) => assert_eq!(s, "u1"),
                    other => panic!("expected string literal key, got {:?}", other),
                }
            }
            other => panic!("expected Expr::GrainRef, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_grain_ref_int_key() {
        let expr = parse_expr(r#"Grain("Counter", 42)"#).unwrap();
        match expr {
            Expr::GrainRef { grain_type, .. } => {
                assert_eq!(grain_type, "Counter");
            }
            other => panic!("expected Expr::GrainRef, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_grain_ref_requires_string_type() {
        let result = parse_expr(r#"Grain(Counter, "u1")"#);
        assert!(result.is_err(), "Grain type must be a string literal");
    }

    #[test]
    fn test_parse_virtual_requires_entity() {
        let source = r#"virtual actor Counter { state count = 0 behavior get() { self.count } }"#;
        let result = parse(source);
        assert!(result.is_err(), "virtual must be followed by entity");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("virtual"),
            "error should mention virtual: {}",
            err
        );
    }

    #[test]
    fn test_parse_persistent_actor_with_state_models() {
        let source = r#"
            persistent actor BankAccount {
                state durable balance: Int = 0
                state local temp: Int = 0
                state event_sourced events: Int = 0
                state crdt viewers: Int = 0
                behavior get() { self.balance }
            }
        "#;
        let ast = parse(source).unwrap();
        match &ast.decls[0] {
            Decl::Actor {
                name,
                persistent,
                state_fields,
                ref behaviors,
                ..
            } => {
                assert_eq!(name, "BankAccount");
                assert!(persistent);
                assert_eq!(state_fields.len(), 4);
                assert_eq!(state_fields[0].0, "balance");
                assert_eq!(state_fields[0].1, StateModel::Durable);
                assert_eq!(state_fields[0].2, Type::int());
                assert_eq!(state_fields[1].1, StateModel::Local);
                assert_eq!(state_fields[2].1, StateModel::EventSourced);
                assert!(matches!(state_fields[3].1, StateModel::Crdt(_)));
                assert_eq!(behaviors.len(), 1);
                assert_eq!(behaviors[0].name, "get");
            }
            _ => panic!("Expected actor declaration"),
        }
    }

    #[test]
    fn test_parse_crdt_state_with_type_selector() {
        let source = r#"
            persistent actor Metrics {
                state crdt gcounter hits: Int = 0
                state crdt pncounter balance: Int = 0
                state crdt orset tags: String = ""
                state crdt lwwregister name: String = ""
                behavior get() { self.hits }
            }
        "#;
        let ast = parse(source).unwrap();
        match &ast.decls[0] {
            Decl::Actor {
                name, state_fields, ..
            } => {
                assert_eq!(name, "Metrics");
                assert_eq!(state_fields.len(), 4);
                assert_eq!(state_fields[0].0, "hits");
                assert!(matches!(
                    state_fields[0].1,
                    StateModel::Crdt(CrdtType::GCounter)
                ));
                assert_eq!(state_fields[1].0, "balance");
                assert!(matches!(
                    state_fields[1].1,
                    StateModel::Crdt(CrdtType::PNCounter)
                ));
                assert_eq!(state_fields[2].0, "tags");
                assert!(matches!(
                    state_fields[2].1,
                    StateModel::Crdt(CrdtType::ORSet)
                ));
                assert_eq!(state_fields[3].0, "name");
                assert!(matches!(
                    state_fields[3].1,
                    StateModel::Crdt(CrdtType::LWWRegister)
                ));
            }
            _ => panic!("Expected actor declaration"),
        }
    }

    #[test]
    fn test_parse_actor_with_initializer() {
        let source = r#"
            persistent actor Counter {
                state durable count: Int = 0
                initial init(start_val: Int) { self.count = start_val }
                behavior inc() { self.count = self.count + 1 }
            }
        "#;
        let ast = parse(source).unwrap();
        match &ast.decls[0] {
            Decl::Actor {
                name, initializer, ..
            } => {
                assert_eq!(name, "Counter");
                let (init_name, params, _body) =
                    initializer.as_ref().expect("should have initializer");
                assert_eq!(init_name, "init");
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "start_val");
            }
            _ => panic!("Expected actor declaration"),
        }
    }

    #[test]
    fn test_parse_spawn_positional_args_and_as() {
        // spawn Foo(a, b) as "my_foo"
        let expr = parse_expr(r#"spawn Foo(1, 2) as "my_foo""#).unwrap();
        match expr {
            Expr::Spawn {
                positional_args,
                register_as,
                ..
            } => {
                let args = positional_args.expect("should have positional args");
                assert_eq!(args.len(), 2);
                assert_eq!(register_as.as_deref(), Some("my_foo"));
            }
            _ => panic!("Expected Spawn"),
        }

        // spawn Foo { x = 1 } as "bar"
        let expr = parse_expr(r#"spawn Foo { x = 1 } as "bar""#).unwrap();
        match expr {
            Expr::Spawn {
                init,
                positional_args,
                register_as,
                ..
            } => {
                assert!(positional_args.is_none());
                assert_eq!(init.len(), 1);
                assert_eq!(init[0].0, "x");
                assert_eq!(register_as.as_deref(), Some("bar"));
            }
            _ => panic!("Expected Spawn"),
        }

        // spawn Foo(1) without as
        let expr = parse_expr("spawn Foo(1)").unwrap();
        match expr {
            Expr::Spawn {
                positional_args,
                register_as,
                ..
            } => {
                assert!(positional_args.is_some());
                assert!(register_as.is_none());
            }
            _ => panic!("Expected Spawn"),
        }
    }
    #[test]
    fn test_parse_record_literal() {
        let ast = parse("{ x: 1, y: 2 }").unwrap();
        match &ast.decls[0] {
            Decl::Function { name, body, .. } if name == "__main" => match body {
                Expr::Record(fields, _) => {
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0].0, "x");
                    assert_eq!(fields[1].0, "y");
                }
                _ => panic!("Expected record literal"),
            },
            _ => panic!("Expected synthetic __main wrapping record literal"),
        }
    }

    #[test]
    fn test_parse_record_pattern() {
        let source = r#"match r { { x: a, y: b } => a + b }"#;
        let expr = parse_expr(source).unwrap();
        match expr {
            Expr::Match { arms, .. } => {
                assert_eq!(arms.len(), 1);
                match &arms[0].0 {
                    Pattern::Record(fields) => {
                        assert_eq!(fields.len(), 2);
                        assert_eq!(fields[0].0, "x");
                        assert_eq!(fields[1].0, "y");
                    }
                    _ => panic!("Expected record pattern"),
                }
            }
            _ => panic!("Expected match expression"),
        }
    }

    #[test]
    fn test_parse_variant_pattern() {
        let source = r#"match o { Some(x) => x | None => 0 }"#;
        let expr = parse_expr(source).unwrap();
        match expr {
            Expr::Match { arms, .. } => {
                assert_eq!(arms.len(), 2);
                match &arms[0].0 {
                    Pattern::Variant(name, Some(_)) => assert_eq!(name, "Some"),
                    _ => panic!("Expected Some variant pattern"),
                }
                match &arms[1].0 {
                    Pattern::Variant(name, None) => assert_eq!(name, "None"),
                    _ => panic!("Expected None variant pattern"),
                }
            }
            _ => panic!("Expected match expression"),
        }
    }

    #[test]
    fn test_parse_handle_with_resume() {
        let source = r#"handle perform E.op() { | E.op() resume => 42 }"#;
        let expr = parse_expr(source).unwrap();
        match expr {
            Expr::Handle { handlers, .. } => {
                assert_eq!(handlers.len(), 1);
                assert_eq!(handlers[0].effect_name, "E");
                assert_eq!(handlers[0].op_name, "op");
                assert!(handlers[0].resume);
            }
            _ => panic!("Expected handle expression"),
        }
    }

    #[test]
    fn test_parse_pipe_operator() {
        let expr = parse_expr("5 |> f").unwrap();
        match expr {
            Expr::Pipe { left, right, .. } => {
                assert!(matches!(left.as_ref(), Expr::Literal(Literal::Int(5), _)));
                assert!(matches!(right.as_ref(), Expr::Var(name, _) if name == "f"));
            }
            _ => panic!("Expected pipe expression"),
        }
    }

    #[test]
    fn test_parse_spawn_link_desugars() {
        // `spawn link A { ... }` desugars in the parser to
        // `let __spawn_ref = spawn A { ... } in { perform Actor.link(__spawn_ref); __spawn_ref }`.
        let expr = parse_expr("spawn link Counter { count = 0 }").unwrap();
        let Expr::Let {
            name, value, body, ..
        } = expr
        else {
            panic!("Expected let from spawn link desugar, got {:?}", expr);
        };
        assert_eq!(name, "__spawn_ref");
        match value.as_ref() {
            Expr::Spawn {
                actor_type, init, ..
            } => {
                assert!(matches!(actor_type.as_ref(), Expr::Var(n, _) if n == "Counter"));
                assert_eq!(init.len(), 1);
                assert_eq!(init[0].0, "count");
            }
            other => panic!("Expected spawn in let value, got {:?}", other),
        }
        match body.as_ref() {
            Expr::Block { exprs, .. } => {
                assert_eq!(exprs.len(), 2);
                match &exprs[0] {
                    Expr::Perform {
                        effect, op, args, ..
                    } => {
                        assert_eq!(effect, "Actor");
                        assert_eq!(op, "link");
                        assert_eq!(args.len(), 1);
                        assert!(matches!(&args[0], Expr::Var(n, _) if n == "__spawn_ref"));
                    }
                    other => panic!("Expected perform Actor.link, got {:?}", other),
                }
                assert!(matches!(&exprs[1], Expr::Var(n, _) if n == "__spawn_ref"));
            }
            other => panic!("Expected block body, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_spawn_monitor_desugars() {
        let expr = parse_expr("spawn monitor Counter { count = 0 }").unwrap();
        let Expr::Let { body, .. } = expr else {
            panic!("Expected let from spawn monitor desugar, got {:?}", expr);
        };
        match body.as_ref() {
            Expr::Block { exprs, .. } => match &exprs[0] {
                Expr::Perform { effect, op, .. } => {
                    assert_eq!(effect, "Actor");
                    assert_eq!(op, "monitor");
                }
                other => panic!("Expected perform Actor.monitor, got {:?}", other),
            },
            other => panic!("Expected block body, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_spawn_plain_not_desugared() {
        let expr = parse_expr("spawn Counter { count = 0 }").unwrap();
        assert!(
            matches!(expr, Expr::Spawn { .. }),
            "plain spawn must stay a Spawn node, got {:?}",
            expr
        );
    }

    #[test]
    fn test_parse_spawn_link_missing_body_errors() {
        assert!(parse_expr("spawn link Counter").is_err());
        assert!(parse_expr("spawn link").is_err());
    }

    #[test]
    fn test_parse_receive_after() {
        let expr = parse_expr("receive { | Msg(x) => x } after 100 => 0").unwrap();
        match expr {
            Expr::Receive { arms, after, .. } => {
                assert_eq!(arms.len(), 1);
                assert_eq!(arms[0].0, "Msg");
                assert_eq!(arms[0].1, vec![Pattern::Var("x".to_string())]);
                let (ms, body) = after.expect("after clause");
                assert!(matches!(ms.as_ref(), Expr::Literal(Literal::Int(100), _)));
                assert!(matches!(body.as_ref(), Expr::Literal(Literal::Int(0), _)));
            }
            _ => panic!("Expected receive expression"),
        }
    }

    #[test]
    fn test_parse_receive_after_dynamic_timeout() {
        // The `after` timeout must accept any expression, not just an Int
        // literal: a variable (or computed expression) timeout must be kept
        // in the AST, never silently dropped to `None`.
        let expr = parse_expr("receive { | Msg(x) => x } after timeout => 0").unwrap();
        match expr {
            Expr::Receive { arms, after, .. } => {
                assert_eq!(arms.len(), 1);
                let (ms, body) = after.expect("dynamic after clause must be kept");
                assert!(
                    matches!(ms.as_ref(), Expr::Var(name, _) if name == "timeout"),
                    "timeout must stay a variable reference, got {:?}",
                    ms
                );
                assert!(matches!(body.as_ref(), Expr::Literal(Literal::Int(0), _)));
            }
            _ => panic!("Expected receive expression"),
        }
        // A computed expression timeout parses the same way.
        let expr = parse_expr("receive { | Msg(x) => x } after t * 10 => 0").unwrap();
        match expr {
            Expr::Receive { after, .. } => {
                let (ms, _) = after.expect("computed after clause must be kept");
                assert!(
                    matches!(ms.as_ref(), Expr::Binary { .. }),
                    "computed timeout must stay an expression, got {:?}",
                    ms
                );
            }
            _ => panic!("Expected receive expression"),
        }
    }

    #[test]
    fn test_parse_receive_without_after() {
        let expr = parse_expr("receive { | Msg(x) => x }").unwrap();
        match expr {
            Expr::Receive { arms, after, .. } => {
                assert_eq!(arms.len(), 1);
                assert!(after.is_none());
            }
            _ => panic!("Expected receive expression"),
        }
    }

    #[test]
    fn test_parse_receive_after_malformed_errors() {
        // Missing `=>` between the timeout expression and the body.
        assert!(parse_expr("receive { | Msg() => 0 } after 100").is_err());
        // Missing timeout expression.
        assert!(parse_expr("receive { | Msg() => 0 } after => 0").is_err());
    }

    #[test]
    fn test_after_stays_a_plain_identifier() {
        // `after` is contextual (only special right after a receive block);
        // elsewhere it remains a usable identifier, e.g. a let binding or a
        // workflow step name (integration_tests has `step after { ... }`).
        let expr = parse_expr("let after = 1 in after + 1").unwrap();
        assert!(
            matches!(&expr, Expr::Let { name, .. } if name == "after"),
            "`after` must still bind as an identifier, got {:?}",
            expr
        );
        let module = parse("workflow W { step after { 1 } }").unwrap();
        assert_eq!(module.decls.len(), 1);
    }

    #[test]
    fn test_parse_standalone_after_desugars_to_receive() {
        // `after ms => body` must desugar to `receive {} after ms => body`.
        let expr = parse_expr("after 5000 => 42").unwrap();
        match expr {
            Expr::Receive { arms, after, .. } => {
                assert!(arms.is_empty(), "standalone after must have empty arms");
                let (ms, body) = after.expect("after clause");
                assert!(matches!(ms.as_ref(), Expr::Literal(Literal::Int(5000), _)));
                assert!(matches!(body.as_ref(), Expr::Literal(Literal::Int(42), _)));
            }
            _ => panic!("Expected Receive from standalone after, got {:?}", expr),
        }
    }

    #[test]
    fn test_parse_standalone_after_with_block_body() {
        let expr = parse_expr("after 100 => { perform IO.print(\"done\") }").unwrap();
        match expr {
            Expr::Receive { arms, after, .. } => {
                assert!(arms.is_empty());
                let (ms, body) = after.expect("after clause");
                assert!(matches!(ms.as_ref(), Expr::Literal(Literal::Int(100), _)));
                assert!(matches!(body.as_ref(), Expr::Block { .. }));
            }
            _ => panic!("Expected Receive, got {:?}", expr),
        }
    }

    #[test]
    fn test_after_still_usable_as_variable_in_expr_position() {
        // `after` as a standalone identifier reference must still work.
        let expr = parse_expr("after").unwrap();
        assert!(
            matches!(expr, Expr::Var(ref name, _) if name == "after"),
            "bare `after` must parse as a variable, got {:?}",
            expr
        );
    }

    #[test]
    fn test_after_still_usable_as_function_call() {
        // `after(x)` must parse as a function call, not as temporal sugar.
        let expr = parse_expr("after(1)").unwrap();
        assert!(
            matches!(expr, Expr::App { ref func, .. } if matches!(func.as_ref(), Expr::Var(ref name, _) if name == "after")),
            "`after(1)` must parse as function call, got {:?}",
            expr
        );
    }

    #[test]
    fn test_parse_perform_keyword_ops() {
        // `link`, `monitor` and `exit` are reserved keywords; they must still
        // parse as effect operation names (BEAM Actor.* builtin effects).
        for (source, expected_op) in [
            ("perform Actor.link(a)", "link"),
            ("perform Actor.monitor(a)", "monitor"),
            ("perform Actor.demonitor(a)", "demonitor"),
            ("perform Actor.unlink(a)", "unlink"),
            ("perform Actor.exit(1)", "exit"),
            ("perform Actor.trap_exit(true)", "trap_exit"),
        ] {
            let expr = parse_expr(source).unwrap();
            match expr {
                Expr::Perform { effect, op, .. } => {
                    assert_eq!(effect, "Actor", "{}", source);
                    assert_eq!(op, expected_op, "{}", source);
                }
                other => panic!("Expected perform for {}, got {:?}", source, other),
            }
        }
    }

    #[test]
    fn test_parse_type_annotation() {
        let expr = parse_expr("(x : Int)").unwrap();
        match expr {
            Expr::TypeAnnotate { expr, ty, .. } => {
                assert!(matches!(expr.as_ref(), Expr::Var(name, _) if name == "x"));
                assert_eq!(ty, Type::Primitive(PrimitiveType::Int));
            }
            _ => panic!("Expected type annotation"),
        }
    }

    #[test]
    fn test_parse_capability_annotation() {
        let expr = parse_expr("x :cap iso").unwrap();
        match expr {
            Expr::CapAnnotate { expr, cap, .. } => {
                assert!(matches!(expr.as_ref(), Expr::Var(name, _) if name == "x"));
                assert_eq!(cap, Capability::Iso);
            }
            _ => panic!("Expected capability annotation"),
        }
    }

    #[test]
    fn test_parse_capability_annotation_lineariso_linear() {
        // Regression: the lexer emits dedicated LinearIso/Linear tokens
        // (not Idents), so the `:cap` annotation path must match the
        // tokens directly. These previously failed with
        // "Expected capability (...), found lineariso".
        for (src, want) in [
            ("x :cap lineariso", Capability::LinearIso),
            ("x :cap linear", Capability::Linear),
        ] {
            let expr = parse_expr(src).unwrap();
            match expr {
                Expr::CapAnnotate { cap, .. } => assert_eq!(cap, want),
                _ => panic!("Expected capability annotation for {src:?}"),
            }
        }
    }

    #[test]
    fn test_parse_cap_ref_value_constructors() {
        // Value-level capability constructors: `&cap expr` must parse to
        // `UnOp::Ref(cap)` for every capability, with bare `&` defaulting
        // to `&ref`. Previously `&lineariso`/`&linear` failed with
        // "Unexpected token in expression".
        for (src, want) in [
            ("&x", Capability::Ref),
            ("&ref x", Capability::Ref),
            ("&iso x", Capability::Iso),
            ("&trn x", Capability::Trn),
            ("&val x", Capability::Val),
            ("&box x", Capability::Box),
            ("&tag x", Capability::Tag),
            ("&lineariso x", Capability::LinearIso),
            ("&linear x", Capability::Linear),
        ] {
            let expr = parse_expr(src).unwrap_or_else(|e| panic!("{src}: {e}"));
            match expr {
                Expr::Unary {
                    op: UnOp::Ref(cap),
                    expr: inner,
                    ..
                } => {
                    assert_eq!(cap, want, "capability for {src:?}");
                    assert!(
                        matches!(inner.as_ref(), Expr::Var(name, _) if name == "x"),
                        "operand for {src:?}"
                    );
                }
                other => panic!("{src:?}: expected Unary Ref, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_parse_let_rec_expression_position() {
        // `let rec f(x) = ... in ...` is expression-position syntax
        // (SPEC2 §6.5 recursive local bindings). At module level it must
        // fall back to the expression path instead of failing with
        // "Expected =" on the parameter list.
        let expr = parse_expr("let rec f(n) = n * 2 in f(3)").unwrap();
        match expr {
            Expr::LetRec { name, params, .. } => {
                assert_eq!(name, "f");
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "n");
            }
            other => panic!("expected LetRec, got {other:?}"),
        }
        // Same through a full module (the __main wrapper).
        let ast = parse("let rec f(n) = n * 2 in f(3)").unwrap();
        assert!(
            ast.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "__main")),
            "module-level let rec must land in __main, got {:?}",
            ast.decls
        );
    }

    #[test]
    fn test_parse_type_decl_alias_bodies() {
        // `type X = <full type>` now accepts any type body (array,
        // primitive, function, reference), not just variants and records.
        // Previously `type Buffer = [Int]` failed with
        // "Expected variant name, found [".
        for (src, want_name) in [
            ("type T = Int", "T"),
            ("type Buffer = [Int]", "Buffer"),
            ("type F = (Int) -> Int", "F"),
            ("type R = &ref Int", "R"),
        ] {
            let ast = parse(src).unwrap();
            match &ast.decls[0] {
                Decl::TypeAlias { name, body, .. } => {
                    assert_eq!(name, want_name, "alias for {src}");
                    assert!(
                        !matches!(body, crate::types::Type::Var(_)),
                        "body must parse"
                    );
                }
                other => panic!("{src}: expected TypeAlias, got {other:?}"),
            }
        }
        // Variants and records still route to their own decl shapes.
        let ast = parse("type Option[T] = Some(T) | None").unwrap();
        assert!(matches!(&ast.decls[0], Decl::VariantType { .. }));
        let ast = parse("type Point = { x: Int, y: Int }").unwrap();
        assert!(matches!(&ast.decls[0], Decl::RecordType { .. }));
        // `Nil` is the canonical empty variant of a sum type and must stay
        // on the variant path even though it is a primitive type name —
        // routing it to the alias path regressed
        // `type Stream[T] = Nil | Cons(...)` (generics_07/typeclass_08
        // conformance cases).
        let ast = parse("type Stream[T] = Nil | Cons((T, Stream[T]))").unwrap();
        match &ast.decls[0] {
            Decl::VariantType { name, variants, .. } => {
                assert_eq!(name, "Stream");
                assert_eq!(variants.len(), 2, "Nil + Cons variants");
                assert_eq!(variants[0].0, "Nil");
                assert_eq!(variants[1].0, "Cons");
            }
            other => panic!("expected VariantType for Nil-led sum type, got {other:?}"),
        }
        // Bare `Nil` still routes to variants (single Nil variant), never
        // to the alias path.
        let ast = parse("type A = Nil").unwrap();
        assert!(matches!(&ast.decls[0], Decl::VariantType { .. }));
        // `type T = Int` stays an alias (primitive, not a variant name).
        let ast = parse("type T = Int").unwrap();
        assert!(matches!(&ast.decls[0], Decl::TypeAlias { .. }));
    }

    #[test]
    fn test_parse_alias_pattern() {
        let expr = parse_expr("match v { n @ Some(x) => n }").unwrap();
        match expr {
            Expr::Match { arms, .. } => match &arms[0].0 {
                Pattern::Alias(name, inner) => {
                    assert_eq!(name, "n");
                    assert!(matches!(inner.as_ref(), Pattern::Variant(v, _) if v == "Some"));
                }
                _ => panic!("Expected alias pattern"),
            },
            _ => panic!("Expected match expression"),
        }
    }

    #[test]
    fn test_parse_error_unexpected_token() {
        let result = parse("fn");
        assert!(result.is_err(), "Expected parse error for bare 'fn'");
    }

    #[test]
    fn test_parse_error_broken_fn_propagates() {
        // A declaration that fails mid-parse must surface its real error
        // instead of retrying the remaining tokens as an expression — `fn 5`
        // must not parse as the expression `5`.
        let result = parse("fn 5");
        assert!(result.is_err(), "Expected parse error for 'fn 5'");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("function name"),
            "Error should be the real declaration error, got: {}",
            msg
        );
    }

    #[test]
    fn test_parse_error_broken_pub_propagates() {
        // `pub` consumed a token before the decl parse failed, so the
        // original error must propagate rather than falling back to `42`.
        let result = parse("pub 42");
        assert!(result.is_err(), "Expected parse error for 'pub 42'");
    }

    #[test]
    fn test_parse_error_broken_module_propagates() {
        // `module Foo` consumed `module`/`Foo` before failing on the missing
        // brace; the error must come from the declaration parse.
        let result = parse("module Foo");
        assert!(result.is_err(), "Expected parse error for 'module Foo'");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("{") || msg.contains("brace"),
            "Error should mention the missing brace, got: {}",
            msg
        );
    }

    #[test]
    fn test_parse_top_level_expression_still_works() {
        // Zero tokens consumed by the decl parse → expression fallback.
        let ast = parse("42").unwrap();
        assert_eq!(ast.decls.len(), 1);
    }

    #[test]
    fn test_parse_doc_comment_before_decl() {
        let ast = parse("/// doc for foo\nfn foo() { 1 }").unwrap();
        assert_eq!(ast.decls.len(), 1);
    }

    #[test]
    fn test_parse_doc_comment_before_expression() {
        let ast = parse("/// doc\n42").unwrap();
        assert_eq!(ast.decls.len(), 1);
    }

    #[test]
    fn test_parse_doc_comment_only_file() {
        let ast = parse("/// nothing but docs").unwrap();
        assert!(ast.decls.is_empty());
    }

    #[test]
    fn test_parse_let_type_annotation() {
        let expr = parse_expr("let x : Int = 1 in x").unwrap();
        match expr {
            Expr::Let { name, ty, .. } => {
                assert_eq!(name, "x");
                assert_eq!(ty, Some(Type::Primitive(PrimitiveType::Int)));
            }
            _ => panic!("Expected let expression"),
        }
    }

    #[test]
    fn test_parse_let_without_annotation() {
        let expr = parse_expr("let x = 1 in x").unwrap();
        match expr {
            Expr::Let { ty, .. } => assert!(ty.is_none()),
            _ => panic!("Expected let expression"),
        }
    }

    #[test]
    fn test_parse_error_missing_arrow_in_effect() {
        let result = parse("effect E { op: Int }");
        assert!(
            result.is_err(),
            "Expected parse error for effect op missing arrow"
        );
    }

    #[test]
    fn test_parse_extern_block() {
        let ast = parse(r#"extern "libm.so.6" { fn sqrt(x: Float) -> Float fn pow(x: Float, y: Float) -> Float }"#).unwrap();
        assert_eq!(ast.decls.len(), 1);
        match &ast.decls[0] {
            Decl::Extern { library, funcs, .. } => {
                assert_eq!(library, "libm.so.6");
                assert_eq!(funcs.len(), 2);
                assert_eq!(funcs[0].name, "sqrt");
                assert_eq!(funcs[0].params, vec![("x".to_string(), Type::float())]);
                assert_eq!(funcs[0].ret, Type::float());
                assert_eq!(funcs[1].name, "pow");
                assert_eq!(
                    funcs[1].params,
                    vec![
                        ("x".to_string(), Type::float()),
                        ("y".to_string(), Type::float())
                    ]
                );
                assert_eq!(funcs[1].ret, Type::float());
            }
            _ => panic!("Expected extern declaration"),
        }
    }

    #[test]
    fn test_parse_extern_empty_block() {
        let ast = parse(r#"extern "empty" {}"#).unwrap();
        match &ast.decls[0] {
            Decl::Extern { library, funcs, .. } => {
                assert_eq!(library, "empty");
                assert!(funcs.is_empty());
            }
            _ => panic!("Expected extern declaration"),
        }
    }

    #[test]
    fn test_parse_extern_missing_param_type_errors() {
        let result = parse(r#"extern "lib" { fn f(x) -> Int }"#);
        assert!(
            result.is_err(),
            "Expected parse error for missing parameter type in extern"
        );
    }

    #[test]
    fn test_parse_workflow_with_steps() {
        let ast =
            parse("workflow PurchaseOrder { step validate { 1 } step charge { 2 } }").unwrap();
        assert_eq!(ast.decls.len(), 1);
        match &ast.decls[0] {
            Decl::Workflow {
                name,
                items,
                compensate,
                ..
            } => {
                assert_eq!(name, "PurchaseOrder");
                assert_eq!(items.len(), 2);
                match (&items[0], &items[1]) {
                    (WorkflowItem::Step(a), WorkflowItem::Step(b)) => {
                        assert_eq!(a.name, "validate");
                        assert_eq!(b.name, "charge");
                    }
                    _ => panic!("Expected two sequential steps"),
                }
                assert!(compensate.is_none());
            }
            _ => panic!("Expected workflow declaration"),
        }
    }

    #[test]
    fn test_parse_workflow_with_parallel_and_compensate() {
        let ast =
            parse("workflow Booking { parallel { step a { 1 } step b { 2 } } compensate { 0 } }")
                .unwrap();
        match &ast.decls[0] {
            Decl::Workflow {
                items, compensate, ..
            } => {
                assert_eq!(items.len(), 1);
                match &items[0] {
                    WorkflowItem::Parallel(branches) => {
                        assert_eq!(branches.len(), 2);
                    }
                    _ => panic!("Expected parallel block"),
                }
                assert!(compensate.is_some());
            }
            _ => panic!("Expected workflow declaration"),
        }
    }

    #[test]
    fn test_parse_workflow_invalid_body_errors() {
        let result = parse("workflow W { fn f() -> Int { 1 } }");
        assert!(
            result.is_err(),
            "Expected parse error for invalid workflow body"
        );
    }

    #[test]
    fn test_parse_tool_annotation() {
        let source = r#"@tool(description: "Adds two integers.")
        pub fn add(x: Int, y: Int) -> Int { x + y }"#;
        let ast = parse(source).unwrap();
        assert_eq!(ast.decls.len(), 1);
        match &ast.decls[0] {
            Decl::Function {
                name,
                annotations,
                public,
                ..
            } => {
                assert_eq!(name, "add");
                assert!(*public);
                assert_eq!(annotations.len(), 1);
                assert_eq!(
                    annotations[0],
                    FunctionAnnotation::Tool {
                        description: "Adds two integers.".to_string(),
                    }
                );
            }
            _ => panic!("Expected function declaration with tool annotation"),
        }
    }

    #[test]
    fn test_parse_agent_full() {
        let source = r#"
            agent MyAgent = {
                model: "gpt-4o",
                system_prompt: "You are helpful.",
                tools: [add, subtract],
                memory: { max_turns: 100 }
            }
        "#;
        let ast = parse(source).unwrap();
        assert_eq!(ast.decls.len(), 1);
        match &ast.decls[0] {
            Decl::Agent {
                name,
                model,
                system_prompt,
                tools,
                memory,
                ..
            } => {
                assert_eq!(name, "MyAgent");
                assert_eq!(model, "gpt-4o");
                assert_eq!(system_prompt.as_deref(), Some("You are helpful."));
                assert_eq!(tools, &["add".to_string(), "subtract".to_string()]);
                assert_eq!(memory.as_ref().unwrap().max_turns, 100);
            }
            _ => panic!("Expected agent declaration"),
        }
    }

    #[test]
    fn test_parse_agent_minimal() {
        let source = r#"agent MyAgent = { model: "gpt-4o" }"#;
        let ast = parse(source).unwrap();
        match &ast.decls[0] {
            Decl::Agent {
                name,
                model,
                system_prompt,
                tools,
                memory,
                ..
            } => {
                assert_eq!(name, "MyAgent");
                assert_eq!(model, "gpt-4o");
                assert!(system_prompt.is_none());
                assert!(tools.is_empty());
                assert_eq!(memory.as_ref().unwrap().max_turns, 50);
            }
            _ => panic!("Expected agent declaration"),
        }
    }

    #[test]
    fn test_parse_agent_missing_model_errors() {
        let result = parse("agent MyAgent = { system_prompt: \"hi\" }");
        assert!(
            result.is_err(),
            "Expected parse error for agent missing model"
        );
    }

    #[test]
    fn test_parse_agent_unknown_field_errors() {
        let result = parse("agent MyAgent = { model: \"x\", unknown: 1 }");
        assert!(
            result.is_err(),
            "Expected parse error for unknown agent field"
        );
    }

    #[test]
    fn test_parse_agent_procedural_memory() {
        let source = r#"
            agent MyAgent = {
                model: "gpt-4o",
                procedural_memory: { namespace: "my_app" }
            }
        "#;
        let ast = parse(source).unwrap();
        match &ast.decls[0] {
            Decl::Agent {
                procedural_memory, ..
            } => {
                assert_eq!(
                    procedural_memory.as_ref().map(|m| m.namespace.as_str()),
                    Some("my_app")
                );
            }
            _ => panic!("Expected agent declaration"),
        }
    }

    #[test]
    fn test_parse_agent_procedural_memory_default_namespace() {
        let source = r#"agent MyAgent = { model: "gpt-4o", procedural_memory: {} }"#;
        let ast = parse(source).unwrap();
        match &ast.decls[0] {
            Decl::Agent {
                procedural_memory, ..
            } => {
                assert_eq!(
                    procedural_memory.as_ref().map(|m| m.namespace.as_str()),
                    Some("default")
                );
            }
            _ => panic!("Expected agent declaration"),
        }
    }

    #[test]
    fn test_parse_state_machine_full_sketch() {
        // The BEAM_PRIMITIVES §4.2 sketch, completed for the implemented
        // grammar: every state is declared with a `state` line (the first is
        // the initial state) and every event target is a declared state.
        let source = r#"
            state_machine TcpConnection {
                state Closed
                state Connecting
                state Connected

                event connect(address): Connecting
                event connection_established: Connected
                event disconnect: Closed

                on_entry Connected {
                    perform IO.print("up")
                }

                on_exit Connected {
                    perform IO.print("down")
                }
            }
        "#;
        let ast = parse(source).unwrap();
        assert_eq!(ast.decls.len(), 1);
        match &ast.decls[0] {
            Decl::StateMachine {
                name,
                states,
                events,
                entry_hooks,
                exit_hooks,
                ..
            } => {
                assert_eq!(name, "TcpConnection");
                assert_eq!(states, &["Closed", "Connecting", "Connected"]);
                assert_eq!(events.len(), 3);
                assert_eq!(events[0].name, "connect");
                assert_eq!(
                    events[0].params,
                    vec![Param {
                        name: "address".to_string(),
                        ty: None,
                        cap: None
                    }]
                );
                assert_eq!(events[0].target, "Connecting");
                assert_eq!(events[1].name, "connection_established");
                assert!(events[1].params.is_empty());
                assert_eq!(events[1].target, "Connected");
                assert_eq!(events[2].name, "disconnect");
                assert_eq!(events[2].target, "Closed");
                assert_eq!(entry_hooks.len(), 1);
                assert_eq!(entry_hooks[0].0, "Connected");
                assert_eq!(exit_hooks.len(), 1);
                assert_eq!(exit_hooks[0].0, "Connected");
            }
            _ => panic!("Expected state_machine declaration"),
        }
    }

    #[test]
    fn test_parse_state_machine_typed_event_params() {
        let source = r#"
            state_machine M {
                state A
                state B
                event go(x: Int, y: String): B
            }
        "#;
        let ast = parse(source).unwrap();
        match &ast.decls[0] {
            Decl::StateMachine { events, .. } => {
                assert_eq!(events.len(), 1);
                assert_eq!(
                    events[0].params,
                    vec![
                        Param::new("x", Some(Type::int())),
                        Param::new("y", Some(Type::string())),
                    ]
                );
            }
            _ => panic!("Expected state_machine declaration"),
        }
    }

    #[test]
    fn test_parse_state_machine_unknown_target_errors() {
        // DECISION (see parse_state_machine docs): unlike gen_statem, an
        // event target must be a declared state — the §4.2 sketch's
        // `event data_received(bytes): handle_data` handler-target form is
        // rejected with a clear error.
        let source = r#"
            state_machine TcpConnection {
                state Closed
                event data_received(bytes): handle_data
            }
        "#;
        let err = parse(source).unwrap_err();
        match err {
            NuError::ParseError { msg, .. } => {
                assert!(msg.contains("unknown state 'handle_data'"), "{}", msg);
                assert!(msg.contains("data_received"), "{}", msg);
                assert!(msg.contains("declared states: Closed"), "{}", msg);
            }
            _ => panic!("Expected ParseError"),
        }
    }

    #[test]
    fn test_parse_state_machine_duplicate_state_errors() {
        let source = "state_machine M { state A state A }";
        let err = parse(source).unwrap_err();
        match err {
            NuError::ParseError { msg, .. } => {
                assert!(msg.contains("duplicate state 'A'"), "{}", msg)
            }
            _ => panic!("Expected ParseError"),
        }
    }

    #[test]
    fn test_parse_state_machine_missing_initial_state_errors() {
        let source = "state_machine M { event go: A }";
        let err = parse(source).unwrap_err();
        match err {
            NuError::ParseError { msg, .. } => {
                assert!(
                    msg.contains("requires at least one 'state <Name>'"),
                    "{}",
                    msg
                )
            }
            _ => panic!("Expected ParseError"),
        }
    }

    #[test]
    fn test_parse_state_machine_hook_unknown_state_errors() {
        let source = r#"
            state_machine M {
                state A
                on_entry B { nil }
            }
        "#;
        let err = parse(source).unwrap_err();
        match err {
            NuError::ParseError { msg, .. } => {
                assert!(
                    msg.contains("on_entry hook references unknown state 'B'"),
                    "{}",
                    msg
                )
            }
            _ => panic!("Expected ParseError"),
        }
    }

    #[test]
    fn test_parse_state_machine_duplicate_event_errors() {
        let source = r#"
            state_machine M {
                state A
                event go: A
                event go: A
            }
        "#;
        let err = parse(source).unwrap_err();
        match err {
            NuError::ParseError { msg, .. } => {
                assert!(msg.contains("duplicate event 'go'"), "{}", msg)
            }
            _ => panic!("Expected ParseError"),
        }
    }

    #[test]
    fn test_parse_state_machine_duplicate_hook_errors() {
        let source = r#"
            state_machine M {
                state A
                on_exit A { nil }
                on_exit A { nil }
            }
        "#;
        let err = parse(source).unwrap_err();
        match err {
            NuError::ParseError { msg, .. } => {
                assert!(
                    msg.contains("duplicate on_exit hook for state 'A'"),
                    "{}",
                    msg
                )
            }
            _ => panic!("Expected ParseError"),
        }
    }

    #[test]
    fn test_parse_entity_with_migration_block() {
        let source = r#"entity BankAccount {
            version: 3
            state balance: Int = 0
            events
                | Deposited(amount: Int)
            migration from 1 to 2 {
                state => { 0 }
                events {
                    | Deposited(amount) => { 1 }
                }
            }
            behavior get() { self.balance }
        }"#;
        let ast = parse(source).unwrap();
        match &ast.decls[0] {
            Decl::Actor {
                name,
                version,
                migrations,
                ..
            } => {
                assert_eq!(name, "BankAccount");
                assert_eq!(*version, 3);
                assert_eq!(migrations.len(), 1);
                assert_eq!(migrations[0].from_version, 1);
                assert_eq!(migrations[0].to_version, 2);
                assert!(migrations[0].state_body.is_some());
                assert_eq!(migrations[0].event_migrations.len(), 1);
                assert_eq!(migrations[0].event_migrations[0].0, "Deposited");
            }
            _ => panic!("Expected Actor decl"),
        }
    }

    #[test]
    fn test_parse_state_machine_unexpected_item_errors() {
        let source = "state_machine M { state A behavior b() { nil } }";
        let err = parse(source).unwrap_err();
        match err {
            NuError::ParseError { msg, .. } => assert!(
                msg.contains("Expected 'state', 'event', 'on_entry', or 'on_exit'"),
                "{}",
                msg
            ),
            _ => panic!("Expected ParseError"),
        }
    }

    // -----------------------------------------------------------------------
    // Named handler declaration: handler name = { | Effect.op(params) resume => body }
    // -----------------------------------------------------------------------
    #[test]
    fn test_parse_named_handler_declaration() {
        let source = r#"
            handler my_io = {
                | IO.print(msg) => 42
                | IO.read() resume => 99
            }
        "#;
        let ast = parse(source).unwrap();
        assert_eq!(ast.decls.len(), 1);
        match &ast.decls[0] {
            Decl::NamedHandler { name, handlers, .. } => {
                assert_eq!(name, "my_io");
                assert_eq!(handlers.len(), 2);
                assert_eq!(handlers[0].effect_name, "IO");
                assert_eq!(handlers[0].op_name, "print");
                assert_eq!(handlers[0].params, vec!["msg"]);
                assert!(!handlers[0].resume);
                assert_eq!(handlers[1].effect_name, "IO");
                assert_eq!(handlers[1].op_name, "read");
                assert!(handlers[1].params.is_empty());
                assert!(handlers[1].resume);
            }
            _ => panic!("Expected NamedHandler declaration, got {:?}", ast.decls[0]),
        }
    }

    #[test]
    fn test_parse_named_handler_without_resume() {
        let source = r#"
            handler simple = {
                | E.op() => 1
            }
        "#;
        let ast = parse(source).unwrap();
        match &ast.decls[0] {
            Decl::NamedHandler { name, handlers, .. } => {
                assert_eq!(name, "simple");
                assert_eq!(handlers.len(), 1);
                assert!(!handlers[0].resume);
            }
            _ => panic!("Expected NamedHandler"),
        }
    }

    // -----------------------------------------------------------------------
    // With block: with handler_name { body_expr }
    // -----------------------------------------------------------------------
    #[test]
    fn test_parse_with_block_resolves_named_handler() {
        // Declare a handler and use it via `with`.
        let source = r#"
            handler h = { | E.op() => 42 }
            with h { perform E.op() }
        "#;
        let ast = parse(source).unwrap();
        assert_eq!(ast.decls.len(), 2);
        // First decl is the handler
        assert!(matches!(ast.decls[0], Decl::NamedHandler { .. }));
        // Second decl is __main containing the with expression
        match &ast.decls[1] {
            Decl::Function { name: _, body, .. } => match body {
                Expr::Handle { handlers, .. } => {
                    assert_eq!(handlers.len(), 1);
                    assert_eq!(handlers[0].effect_name, "E");
                    assert_eq!(handlers[0].op_name, "op");
                }
                _ => panic!("Expected Handle expression, got {:?}", body),
            },
            _ => panic!("Expected __main function, got {:?}", ast.decls[1]),
        }
    }

    #[test]
    fn test_parse_with_block_undefined_handler_errors() {
        let source = r#"
            with nonexistent { 42 }
        "#;
        let err = parse(source).unwrap_err();
        match err {
            NuError::ParseError { msg, .. } => {
                assert!(
                    msg.contains("undefined handler"),
                    "expected 'undefined handler' error, got: {}",
                    msg
                );
            }
            _ => panic!("Expected ParseError, got {:?}", err),
        }
    }

    // -----------------------------------------------------------------------
    // Consume expression: consume var
    // -----------------------------------------------------------------------
    #[test]
    fn test_parse_consume_expr() {
        let source = "let x = 42 in consume x";
        let expr = parse_expr(source).unwrap();
        match expr {
            Expr::Let { body, .. } => match body.as_ref() {
                Expr::Consume { expr: inner, .. } => {
                    assert!(matches!(inner.as_ref(), Expr::Var(name, _) if name == "x"));
                }
                _ => panic!("Expected Consume in body, got {:?}", body),
            },
            _ => panic!("Expected Let expression, got {:?}", expr),
        }
    }
    // -----------------------------------------------------------------------
    // Until expression: until <condition> => <body>  (polling loop sugar)
    // -----------------------------------------------------------------------
    #[test]
    fn test_parse_until_desugars_to_polling_loop() {
        let source = "until x > 0 => 42";
        let expr = parse_expr(source).unwrap();
        // Should desugar to: let __until_poll = 100 in let __until_loop = fn() { ... } in __until_loop()
        match expr {
            Expr::Let { name, body, .. } => {
                assert_eq!(name, "__until_poll");
                match body.as_ref() {
                    Expr::Let { name, .. } => {
                        assert_eq!(name, "__until_loop");
                    }
                    _ => panic!("Expected Let for __until_loop, got {:?}", body),
                }
            }
            _ => panic!("Expected Let for __until_poll, got {:?}", expr),
        }
    }

    #[test]
    fn test_parse_until_with_poll_clause() {
        let source = "until done poll 50 => perform IO.print(\"ok\")";
        let expr = parse_expr(source).unwrap();
        match expr {
            Expr::Let { name, value, .. } => {
                assert_eq!(name, "__until_poll");
                // The poll value should be 50
                assert!(matches!(value.as_ref(), Expr::Literal(Literal::Int(50), _)));
            }
            _ => panic!("Expected Let for __until_poll"),
        }
    }

    // -----------------------------------------------------------------------
    // Range expressions (..)
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_range_simple() {
        let expr = parse_expr("0 .. 5").unwrap();
        match expr {
            Expr::Binary {
                op: BinOp::Range,
                left,
                right,
                ..
            } => {
                assert!(matches!(left.as_ref(), Expr::Literal(Literal::Int(0), _)));
                assert!(matches!(right.as_ref(), Expr::Literal(Literal::Int(5), _)));
            }
            _ => panic!("Expected Range binary, got {:?}", expr),
        }
    }

    #[test]
    fn test_parse_range_with_arithmetic() {
        // a + b .. c + d should parse as (a + b) .. (c + d)
        let expr = parse_expr("a + b .. c + d").unwrap();
        match expr {
            Expr::Binary {
                op: BinOp::Range,
                left,
                right,
                ..
            } => {
                assert!(matches!(left.as_ref(), Expr::Binary { op: BinOp::Add, .. }));
                assert!(matches!(
                    right.as_ref(),
                    Expr::Binary { op: BinOp::Add, .. }
                ));
            }
            _ => panic!("Expected Range binary, got {:?}", expr),
        }
    }

    #[test]
    fn test_parse_record_update_still_works() {
        let source = "{ p .. y = 9 }";
        let expr = parse_expr(source).unwrap();
        match expr {
            Expr::RecordUpdate { base, fields, .. } => {
                assert!(matches!(base.as_ref(), Expr::Var(name, _) if name == "p"));
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].0, "y");
                assert!(matches!(fields[0].1, Expr::Literal(Literal::Int(9), _)));
            }
            _ => panic!("Expected RecordUpdate, got {:?}", expr),
        }
    }

    #[test]
    fn test_parse_block_with_range() {
        // { a .. b } should parse as a block containing a range expression
        let source = "{ a .. b }";
        let expr = parse_expr(source).unwrap();
        match expr {
            Expr::Block { exprs, .. } => {
                assert_eq!(exprs.len(), 1);
                assert!(matches!(
                    &exprs[0],
                    Expr::Binary {
                        op: BinOp::Range,
                        ..
                    }
                ));
            }
            _ => panic!("Expected Block with range, got {:?}", expr),
        }
    }

    #[test]
    fn test_parse_for_in_range() {
        let source = "for i in 0 .. 5 { i }";
        let expr = parse_expr(source).unwrap();
        match expr {
            Expr::For { var, iterable, .. } => {
                assert_eq!(var, "i");
                assert!(matches!(
                    iterable.as_ref(),
                    Expr::Binary {
                        op: BinOp::Range,
                        ..
                    }
                ));
            }
            _ => panic!("Expected For with range, got {:?}", expr),
        }
    }

    #[test]
    fn test_parse_par_block() {
        let source = "par { a + 1; b * 2 }";
        let expr = parse_expr(source).unwrap();
        match expr {
            Expr::Par { exprs, span } => {
                assert_eq!(exprs.len(), 2);
                assert!(span.start < span.end, "par should carry a source span");
            }
            other => panic!("Expected Par, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_par_requires_brace() {
        let result = parse_expr("par");
        assert!(result.is_err(), "bare 'par' must be a parse error");
    }
}
