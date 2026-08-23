//! Shared type definitions used across all Nulang compiler and runtime modules.

use crate::type_ir::NtirNode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
// Fast hashing for compiler-internal maps (keys are not attacker-controlled).
type FxHashMap<K, V> =
    std::collections::HashMap<K, V, std::hash::BuildHasherDefault<rustc_hash::FxHasher>>;
type FxHashSet<T> =
    std::collections::HashSet<T, std::hash::BuildHasherDefault<rustc_hash::FxHasher>>;

// ---------------------------------------------------------------------------
// Type Variables & Regions
// ---------------------------------------------------------------------------

static TYPE_VAR_COUNTER: AtomicU64 = AtomicU64::new(1);
static REGION_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TypeVar(pub u64);

impl std::fmt::Display for TypeVar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "'_")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Region(pub u64);

impl TypeVar {
    pub fn fresh() -> Self {
        TypeVar(TYPE_VAR_COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

impl Region {
    pub fn fresh() -> Region {
        Region(REGION_COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

// ---------------------------------------------------------------------------
// Primitive Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PrimitiveType {
    Int,
    Float,
    Bool,
    String,
    Nil,
    Unit,
    Never,
    Address, // Actor address
}

// ---------------------------------------------------------------------------
// Reference Capabilities (Pony-inspired)
// ---------------------------------------------------------------------------

/// Reference capability lattice:
/// ```text
///       LinearIso
///       /      \
///     Iso     Linear
///     / \      /
///   Trn Val<--/
///    |   |
///   Ref Box
///     \ /
///     Tag
/// ```
/// Subtyping: lineariso <: iso <: trn <: ref <: box, linear <: val <: box, ref <: tag, val <: tag, box <: tag
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    LinearIso, // Unique ownership with linear type tracking (provably consumed exactly once)
    Linear,    // Immutable + linear-tracked + remote-sendable ("linear Val")
    Iso,       // Unique ownership (can be sent to another actor)
    Trn,       // Unique writer (can be recovered to iso)
    Ref,       // Shared read/write reference
    Val,       // Immutable shared reference (sendable)
    Box,       // Read-only reference (any cap except tag can be read as box)
    Tag,       // Opaque identity only (tagged pointer, no dereference)
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Capability::LinearIso => write!(f, "lineariso"),
            Capability::Linear => write!(f, "linear"),
            Capability::Iso => write!(f, "iso"),
            Capability::Trn => write!(f, "trn"),
            Capability::Ref => write!(f, "ref"),
            Capability::Val => write!(f, "val"),
            Capability::Box => write!(f, "box"),
            Capability::Tag => write!(f, "tag"),
        }
    }
}

impl Capability {
    /// Least upper bound (join) of two capabilities.
    ///
    /// LinearIso behaves like Iso in joins, except LinearIso ⊔ LinearIso = LinearIso.
    pub fn join(self, other: Capability) -> Capability {
        use Capability::*;

        match (self, other) {
            // LinearIso joins: LinearIso + LinearIso stays LinearIso
            (LinearIso, LinearIso) => LinearIso,
            // LinearIso + Iso promotes to Iso (linear obligation can be discharged)
            (LinearIso, Iso) | (Iso, LinearIso) => Iso,
            // LinearIso with Trn (same as Iso with Trn)
            (LinearIso, Trn) | (Trn, LinearIso) => Trn,
            // LinearIso with Ref (same as Iso with Ref)
            (LinearIso, Ref) | (Ref, LinearIso) => Ref,
            // LinearIso with Val (same as Iso with Val)
            (LinearIso, Val) | (Val, LinearIso) => Val,
            // LinearIso with Box (same as Iso with Box)
            (LinearIso, Box) | (Box, LinearIso) => Box,
            // LinearIso with Tag (same as Iso with Tag)
            (LinearIso, Tag) | (Tag, LinearIso) => LinearIso,

            // Linear joins: Linear behaves like Val except Linear join Linear = Linear
            (Linear, Linear) => Linear,
            (Linear, Val) | (Val, Linear) => Val,
            (Linear, LinearIso) | (LinearIso, Linear) => Val,
            (Linear, Iso) | (Iso, Linear) => Val,
            (Linear, Trn) | (Trn, Linear) => Val,
            (Linear, Ref) | (Ref, Linear) => Box,
            (Linear, Box) | (Box, Linear) => Box,
            (Linear, Tag) | (Tag, Linear) => Linear,

            // Original capability joins (unchanged)
            (Iso, Iso) => Iso,
            (Iso, Trn) | (Trn, Iso) | (Trn, Trn) => Trn,
            (Iso, Ref) | (Ref, Iso) | (Trn, Ref) | (Ref, Trn) | (Ref, Ref) => Ref,
            (Iso, Val) | (Val, Iso) | (Trn, Val) | (Val, Trn) | (Val, Val) => Val,
            (Ref, Val) | (Val, Ref) => Box,
            (Iso, Box)
            | (Box, Iso)
            | (Trn, Box)
            | (Box, Trn)
            | (Ref, Box)
            | (Box, Ref)
            | (Val, Box)
            | (Box, Val)
            | (Box, Box) => Box,
            (Tag, c) | (c, Tag) if c == Tag => Tag,
            (Tag, c) | (c, Tag) => c, // tag is bottom-ish for read-only
        }
    }

    /// Check if self <: other (self is a subtype of other).
    pub fn is_subtype_of(self, other: Capability) -> bool {
        self.join(other) == other
    }

    /// Can this capability be sent to another actor?
    pub fn is_sendable(self) -> bool {
        matches!(
            self,
            Capability::LinearIso
                | Capability::Linear
                | Capability::Iso
                | Capability::Val
                | Capability::Tag
        )
    }

    /// Can this capability be sent over the network (serializable)?
    pub fn is_remote_sendable(self) -> bool {
        matches!(self, Capability::Linear | Capability::Val | Capability::Tag)
    }

    /// Can this capability be read through?
    pub fn is_readable(self) -> bool {
        !matches!(self, Capability::Tag)
    }

    /// Can this capability be written through?
    pub fn is_writable(self) -> bool {
        matches!(
            self,
            Capability::LinearIso | Capability::Iso | Capability::Trn | Capability::Ref
        )
    }

    /// Is this a linear capability (requires exactly-one consumption tracking)?
    pub fn is_linear(self) -> bool {
        matches!(self, Capability::LinearIso | Capability::Linear)
    }

    /// Discharge linear tracking: LinearIso→Iso, Linear→Val.
    pub fn discharge_linear(self) -> Capability {
        match self {
            Capability::LinearIso => Capability::Iso,
            Capability::Linear => Capability::Val,
            other => other,
        }
    }

    #[deprecated(note = "use discharge_linear")]
    pub fn promote_to_iso(self) -> Capability {
        self.discharge_linear()
    }
}

// ---------------------------------------------------------------------------
// Effect Rows (Koka-inspired, row polymorphism)
// ---------------------------------------------------------------------------

/// A built-in or user-defined effect.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Effect {
    IO,
    Net,
    String,
    FS,
    Rand,
    Time,
    Spawn,
    Send,
    Receive,
    Migrate,
    STM,
    Async,
    Inference,
    Cost,
    Event,
    Array,
    FFI,
    Test,
    DB,
    Python,
    Env,
    Process,
    System,
    /// Web framework: produce HTML output (server-side rendering / static emit).
    Render,
    /// Web framework: read the current HTTP request.
    Request,
    /// Web framework: write the HTTP response.
    Respond,
    /// Web framework: SSE / WebSocket push (subsumes Net at runtime).
    Realtime,
    /// Web framework: browser-only DOM / signal operations.
    Client,
    /// Web framework: built-in host operations (route, html, redirect, ...).
    Web,
    UserDefined(String),
}

impl std::fmt::Display for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effect::IO => write!(f, "IO"),
            Effect::Net => write!(f, "Net"),
            Effect::String => write!(f, "String"),
            Effect::FS => write!(f, "FS"),
            Effect::Array => write!(f, "Array"),
            Effect::Rand => write!(f, "Rand"),
            Effect::Time => write!(f, "Time"),
            Effect::Spawn => write!(f, "Spawn"),
            Effect::Send => write!(f, "Send"),
            Effect::Receive => write!(f, "Receive"),
            Effect::Migrate => write!(f, "Migrate"),
            Effect::STM => write!(f, "STM"),
            Effect::Async => write!(f, "Async"),
            Effect::Inference => write!(f, "Inference"),
            Effect::Cost => write!(f, "Cost"),
            Effect::Event => write!(f, "Event"),
            Effect::FFI => write!(f, "FFI"),
            Effect::Test => write!(f, "Test"),
            Effect::DB => write!(f, "DB"),
            Effect::Python => write!(f, "Python"),
            Effect::Env => write!(f, "Env"),
            Effect::Process => write!(f, "Process"),
            Effect::System => write!(f, "System"),
            Effect::Render => write!(f, "Render"),
            Effect::Request => write!(f, "Request"),
            Effect::Respond => write!(f, "Respond"),
            Effect::Realtime => write!(f, "Realtime"),
            Effect::Client => write!(f, "Client"),
            Effect::Web => write!(f, "Web"),
            Effect::UserDefined(s) => write!(f, "{}", s),
        }
    }
}

/// Compile-time placement hint for web-framework functions.
/// Stored on HIR/MIR function metadata; default inference is based on the
/// function's effect row and purity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Placement {
    /// Static site generation: no request effects, pure render.
    Static,
    /// Server-side request handler.
    Server,
    /// Edge compute (CDN worker).
    Edge,
    /// Browser-only client code.
    Client,
    /// Actor-based backend.
    Actor,
    /// Workflow (durable step function).
    Workflow,
}

impl std::fmt::Display for Placement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Placement::Static => write!(f, "static"),
            Placement::Server => write!(f, "server"),
            Placement::Edge => write!(f, "edge"),
            Placement::Client => write!(f, "client"),
            Placement::Actor => write!(f, "actor"),
            Placement::Workflow => write!(f, "workflow"),
        }
    }
}

/// Effect row: either closed (fixed set) or open (set + row variable).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EffectRow {
    Closed(Vec<Effect>),
    Open(Vec<Effect>, Region),
}

impl std::fmt::Display for EffectRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EffectRow::Closed(effects) => {
                write!(f, "{{")?;
                for (i, e) in effects.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", e)?;
                }
                write!(f, "}}")
            }
            EffectRow::Open(effects, _) => {
                write!(f, "{{")?;
                for (i, e) in effects.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", e)?;
                }
                if !effects.is_empty() {
                    write!(f, ", ")?;
                }
                write!(f, "..}}")
            }
        }
    }
}

impl EffectRow {
    pub fn empty() -> Self {
        EffectRow::Closed(vec![])
    }

    pub fn singleton(e: Effect) -> Self {
        EffectRow::Closed(vec![e])
    }

    /// Row concatenation.
    pub fn combine(self, other: EffectRow) -> EffectRow {
        match (self, other) {
            (EffectRow::Closed(mut a), EffectRow::Closed(b)) => {
                a.extend(b);
                EffectRow::Closed(a)
            }
            (EffectRow::Closed(mut a), EffectRow::Open(b, r))
            | (EffectRow::Open(mut a, r), EffectRow::Closed(b)) => {
                a.extend(b);
                EffectRow::Open(a, r)
            }
            (EffectRow::Open(mut a, r1), EffectRow::Open(b, _)) => {
                // Open rows share the same row variable convention
                a.extend(b);
                EffectRow::Open(a, r1)
            }
        }
    }

    /// Check if a specific effect is in this row.
    pub fn contains(&self, eff: &Effect) -> bool {
        match self {
            EffectRow::Closed(effects) => effects.contains(eff),
            EffectRow::Open(effects, _) => effects.contains(eff),
        }
    }

    /// Remove an effect from this row (for handled effects).
    pub fn remove(self, eff: &Effect) -> EffectRow {
        match self {
            EffectRow::Closed(effects) => {
                EffectRow::Closed(effects.into_iter().filter(|e| e != eff).collect())
            }
            EffectRow::Open(effects, r) => {
                EffectRow::Open(effects.into_iter().filter(|e| e != eff).collect(), r)
            }
        }
    }

    /// Get the set of effects (ignoring row variable).
    pub fn effects(&self) -> &[Effect] {
        match self {
            EffectRow::Closed(effects) => effects,
            EffectRow::Open(effects, _) => effects,
        }
    }
}

// ---------------------------------------------------------------------------
// Core Type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    /// Type variable (for inference)
    Var(TypeVar),
    /// Primitive type
    Primitive(PrimitiveType),
    /// Tuple (A, B, ...)
    Tuple(Vec<Type>),
    /// Record { field: Type, ... }
    ///
    /// A record whose field list ends with the reserved pseudo-field
    /// [`RECORD_ROW_TAIL_FIELD`] is an *open* record: the pseudo-field's type
    /// is a row variable standing for "possibly more fields". Records from
    /// literals and annotations are always closed (no tail).
    Record(Vec<(String, Type)>),
    /// Variant Type1 | Type2 | ...
    Variant(Vec<(String, Option<Type>)>),
    /// Array [Type]
    Array(Box<Type>),
    /// Function: arg type -> return type with effect row and capability
    Function {
        param: Box<Type>,
        ret: Box<Type>,
        effect: EffectRow,
        cap: Capability,
    },
    /// Actor[State, Behavior]
    Actor {
        state: Box<Type>,
        behavior: Box<Type>,
    },
    /// Generic type application: List[Int], Map[String, Int]
    App {
        constructor: Box<Type>,
        args: Vec<Type>,
    },
    /// Reference type with capability: &cap Type
    Reference { cap: Capability, inner: Box<Type> },
    /// Existential / type scheme: forall vars. Type
    Scheme { vars: Vec<TypeVar>, body: Box<Type> },
    /// Nominal (opaque) type: `opaque type UserId = Int`.
    /// Distinct from its underlying type at compile time, erases at runtime.
    Nominal { name: String, underlying: Box<Type> },
    /// Skolem type constant: a rigid placeholder for a type parameter
    /// during function body checking.  Skolems unify only with themselves
    /// (never with concrete types), preventing the function body from
    /// pinning a generic type parameter to a specific type.
    Skolem(u64),
}

impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Var(v) => write!(f, "{}", v),
            Type::Primitive(p) => match p {
                PrimitiveType::Int => write!(f, "Int"),
                PrimitiveType::Float => write!(f, "Float"),
                PrimitiveType::Bool => write!(f, "Bool"),
                PrimitiveType::String => write!(f, "String"),
                PrimitiveType::Unit => write!(f, "Unit"),
                PrimitiveType::Nil => write!(f, "Nil"),
                PrimitiveType::Never => write!(f, "Never"),
                PrimitiveType::Address => write!(f, "Address"),
            },
            Type::Tuple(ts) => {
                write!(f, "(")?;
                for (i, t) in ts.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", t)?;
                }
                write!(f, ")")
            }
            Type::Record(fs) => {
                write!(f, "{{ ")?;
                for (i, (n, t)) in fs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", n, t)?;
                }
                write!(f, " }}")
            }
            Type::Variant(vs) => {
                for (i, (n, t)) in vs.iter().enumerate() {
                    if i > 0 {
                        write!(f, " | ")?;
                    }
                    match t {
                        Some(t) => write!(f, "{} {}", n, t)?,
                        None => write!(f, "{}", n)?,
                    }
                }
                Ok(())
            }
            Type::Array(t) => write!(f, "[{}]", t),
            Type::Function {
                param,
                ret,
                effect: _,
                cap: _,
            } => {
                write!(f, "{} -> {}", param, ret)
            }
            Type::Actor { state, behavior } => {
                write!(f, "Actor[{}, {}]", state, behavior)
            }
            Type::App { constructor, args } => {
                write!(f, "{}", constructor)?;
                if !args.is_empty() {
                    write!(f, "[")?;
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", a)?;
                    }
                    write!(f, "]")?;
                }
                Ok(())
            }
            Type::Reference { cap, inner } => write!(f, "&{} {}", cap, inner),
            Type::Scheme { vars, body } => {
                write!(f, "forall ")?;
                for (i, v) in vars.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "'t{}", v.0)?;
                }
                write!(f, ". {}", body)
            }
            Type::Nominal { name, .. } => write!(f, "{}", name),
            Type::Skolem(id) => write!(f, "'t{}", id),
        }
    }
}

/// Reserved pseudo-field name carrying the *row tail* of an open record type.
///
/// Record row polymorphism is encoded without changing the shape of
/// `Type::Record(Vec<(String, Type)>)` — exhaustive matches on `Type` exist
/// across the crate (`main.rs`, `repl.rs`, `mir_codegen.rs`, `tool_schema.rs`),
/// so the representation stays additive. An open record `{ x: a | rho }` is
/// represented as `Record([("x", a), ("..", Var(rho))])`. The name `".."`
/// can never collide with a user field: record field names are parsed with
/// `expect_ident`, and `".."` is not a valid identifier.
///
/// The tail's type is a fresh `Type::Var` when produced; record unification
/// may substitute it with an open record (row extension) or a closed record
/// (row closing). Because the row variable is an ordinary type variable in an
/// ordinary field, `free_vars`, `ref_free_vars`, substitution, the occurs
/// check, and generalization all handle it with no special casing.
pub const RECORD_ROW_TAIL_FIELD: &str = "..";

impl Type {
    /// Convert to an NTIR structural representation for content-addressed hashing.
    pub fn to_ntir(&self) -> NtirNode {
        self.to_ntir_with_stack(&mut Vec::new())
    }

    fn to_ntir_with_stack(&self, stack: &mut Vec<TypeVar>) -> NtirNode {
        if let Type::Var(v) = self {
            if let Some(pos) = stack.iter().rev().position(|x| x == v) {
                return NtirNode::Cycle(pos as u64);
            }
        }

        let push_var = if let Type::Var(v) = self {
            stack.push(*v);
            true
        } else {
            false
        };

        let res = match self {
            Type::Var(_) => NtirNode::Primitive(PrimitiveType::Unit),
            Type::Primitive(p) => NtirNode::Primitive(p.clone()),
            Type::Tuple(ts) => {
                NtirNode::Tuple(ts.iter().map(|t| t.to_ntir_with_stack(stack)).collect())
            }
            Type::Record(fs) => {
                let mut mapped: Vec<_> = fs
                    .iter()
                    .map(|(n, t)| (n.clone(), t.to_ntir_with_stack(stack)))
                    .collect();
                mapped.sort_by(|a, b| a.0.cmp(&b.0));
                NtirNode::Record(mapped)
            }
            Type::Variant(vs) => {
                let mut mapped: Vec<_> = vs
                    .iter()
                    .map(|(n, t_opt)| {
                        let t_ntir = match t_opt {
                            Some(t) => t.to_ntir_with_stack(stack),
                            None => NtirNode::Primitive(PrimitiveType::Unit),
                        };
                        (n.clone(), t_ntir)
                    })
                    .collect();
                mapped.sort_by(|a, b| a.0.cmp(&b.0));
                NtirNode::Variant(mapped)
            }
            Type::Array(inner) => NtirNode::Tuple(vec![inner.to_ntir_with_stack(stack)]),
            Type::Function {
                param, ret, cap, ..
            } => NtirNode::Capability(
                *cap,
                Box::new(NtirNode::Tuple(vec![
                    param.to_ntir_with_stack(stack),
                    ret.to_ntir_with_stack(stack),
                ])),
            ),
            Type::Actor { state, behavior } => NtirNode::Tuple(vec![
                state.to_ntir_with_stack(stack),
                behavior.to_ntir_with_stack(stack),
            ]),
            Type::App { constructor, args } => {
                let mut elems = vec![constructor.to_ntir_with_stack(stack)];
                for a in args {
                    elems.push(a.to_ntir_with_stack(stack));
                }
                NtirNode::Tuple(elems)
            }
            Type::Reference { cap, inner } => {
                NtirNode::Capability(*cap, Box::new(inner.to_ntir_with_stack(stack)))
            }
            Type::Scheme { body, .. } => body.to_ntir_with_stack(stack),
            Type::Nominal { underlying, .. } => underlying.to_ntir_with_stack(stack),
            Type::Skolem(_) => NtirNode::Primitive(PrimitiveType::Unit),
        };

        if push_var {
            stack.pop();
        }
        res
    }

    /// True if the type contains no free type variables.
    pub fn is_ground(&self) -> bool {
        let mut fv = Vec::new();
        self.collect_free_vars(&mut fv);
        fv.is_empty()
    }
    pub fn int() -> Type {
        Type::Primitive(PrimitiveType::Int)
    }

    /// A closed record type: exactly the given fields. Record literals and
    /// annotations are always closed.
    pub fn record(fields: Vec<(String, Type)>) -> Type {
        Type::Record(fields)
    }

    /// An open record type: the given fields plus a fresh row variable
    /// standing for "possibly more fields". Produced by field access on a
    /// record of not-yet-known shape; see [`RECORD_ROW_TAIL_FIELD`].
    pub fn record_open(fields: Vec<(String, Type)>, row: TypeVar) -> Type {
        let mut fields = fields;
        fields.push((RECORD_ROW_TAIL_FIELD.to_string(), Type::Var(row)));
        Type::Record(fields)
    }

    pub fn float() -> Type {
        Type::Primitive(PrimitiveType::Float)
    }
    pub fn bool() -> Type {
        Type::Primitive(PrimitiveType::Bool)
    }
    pub fn string() -> Type {
        Type::Primitive(PrimitiveType::String)
    }
    pub fn nil() -> Type {
        Type::Primitive(PrimitiveType::Nil)
    }

    pub fn unit() -> Type {
        Type::Primitive(PrimitiveType::Unit)
    }

    /// Free type variables in this type.
    pub fn free_vars(&self) -> Vec<TypeVar> {
        let mut vars = vec![];
        self.collect_free_vars(&mut vars);
        vars.sort_by_key(|v| v.0);
        vars.dedup_by_key(|v| v.0);
        vars
    }

    /// Free type variables that occur underneath a `Reference` constructor.
    ///
    /// Used for the value restriction at generalization: a reference cell is
    /// created once at binding time and shared by every use of the binding, so
    /// quantifying a variable under a `Reference` would let one cell be used at
    /// incompatible types. Function types are not descended into — a reference
    /// in a function's parameter or return type is created per call, so
    /// quantifying it is sound.
    pub fn ref_free_vars(&self) -> Vec<TypeVar> {
        let mut vars = vec![];
        self.collect_ref_free_vars(&mut vars);
        vars.sort_by_key(|v| v.0);
        vars.dedup_by_key(|v| v.0);
        vars
    }

    fn collect_ref_free_vars(&self, acc: &mut Vec<TypeVar>) {
        match self {
            // The shared cell: every free variable inside must stay monomorphic.
            Type::Reference { inner, .. } => inner.collect_free_vars(acc),
            // Function values are created per call — refs in their types are safe.
            Type::Function { .. } => {}
            Type::Tuple(ts) => ts.iter().for_each(|t| t.collect_ref_free_vars(acc)),
            Type::Record(fs) => fs.iter().for_each(|(_, t)| t.collect_ref_free_vars(acc)),
            Type::Variant(vs) => vs.iter().for_each(|(_, t)| {
                if let Some(t) = t {
                    t.collect_ref_free_vars(acc)
                }
            }),
            Type::Array(t) => t.collect_ref_free_vars(acc),
            Type::Actor { state, behavior } => {
                state.collect_ref_free_vars(acc);
                behavior.collect_ref_free_vars(acc);
            }
            Type::App { constructor, args } => {
                constructor.collect_ref_free_vars(acc);
                args.iter().for_each(|a| a.collect_ref_free_vars(acc));
            }
            Type::Scheme { body, .. } => body.collect_ref_free_vars(acc),
            Type::Nominal { underlying, .. } => underlying.collect_ref_free_vars(acc),
            Type::Var(_) | Type::Primitive(_) | Type::Skolem(_) => {}
        }
    }

    fn collect_free_vars(&self, acc: &mut Vec<TypeVar>) {
        match self {
            Type::Var(v) => acc.push(*v),
            Type::Primitive(_) => {}
            Type::Tuple(ts) => ts.iter().for_each(|t| t.collect_free_vars(acc)),
            Type::Record(fs) => fs.iter().for_each(|(_, t)| t.collect_free_vars(acc)),
            Type::Variant(vs) => vs.iter().for_each(|(_, t)| {
                if let Some(t) = t {
                    t.collect_free_vars(acc)
                }
            }),
            Type::Array(t) => t.collect_free_vars(acc),
            Type::Function { param, ret, .. } => {
                param.collect_free_vars(acc);
                ret.collect_free_vars(acc);
            }
            Type::Actor { state, behavior } => {
                state.collect_free_vars(acc);
                behavior.collect_free_vars(acc);
            }
            Type::App { constructor, args } => {
                constructor.collect_free_vars(acc);
                args.iter().for_each(|a| a.collect_free_vars(acc));
            }
            Type::Reference { inner, .. } => inner.collect_free_vars(acc),
            Type::Scheme { vars, body } => {
                body.collect_free_vars(acc);
                // Remove bound vars
                acc.retain(|v| !vars.contains(v));
            }
            Type::Nominal { underlying, .. } => underlying.collect_free_vars(acc),
            Type::Skolem(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Type Context (Gamma)
// ---------------------------------------------------------------------------

/// Typing context: maps variable names to their (type, capability) bindings.
///
/// Linear (`LinearIso`) consumption is tracked separately by the capability
/// analyzer.
#[derive(Debug, Clone, Default)]
pub struct TypeContext {
    bindings: HashMap<String, (Type, Capability, bool)>,
    /// Names hidden by an enclosing `hide` / `seal except` directive. A hidden
    /// name resolves as unbound. Local bindings created inside the directive's
    /// body are NOT hidden — the set is snapshotted at directive entry.
    hidden: FxHashSet<String>,
    /// Event declarations from the enclosing entity (if any). Used by the
    /// typechecker to validate `emit EventName(args)` calls. Stored as
    /// `(event_name, [(param_name, param_type)])`.
    pub entity_events: Option<Vec<(String, Vec<(String, Type)>)>>,
    /// Typeclass constraints on type variables: maps each type variable to
    /// the list of class names it must satisfy. Populated when a function
    /// signature declares `fn f[T: Eq, Ord](...)` or through `where` clauses.
    /// Checked by instance-lookup (B.4) when a concrete type is substituted.
    pub constraints: FxHashMap<TypeVar, Vec<String>>,
}

impl TypeContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a variable name to a type, capability, and mutability.
    pub fn bind(&mut self, name: impl Into<String>, ty: Type, cap: Capability, mutable: bool) {
        let name = name.into();
        self.bindings.insert(name, (ty, cap, mutable));
    }

    /// Look up a variable's type, capability, and mutability.
    pub fn lookup(&self, name: &str) -> Option<&(Type, Capability, bool)> {
        if self.hidden.contains(name) {
            return None;
        }
        self.bindings.get(name)
    }

    /// Hide the given names from resolution (`hide a, b { body }`).
    pub fn hide_names(&mut self, names: &[String]) {
        self.hidden.extend(names.iter().cloned());
    }

    /// Hide every currently-bound name except the allowlist
    /// (`seal except a, b { body }`).
    pub fn seal_except(&mut self, names: &[String]) {
        let allow: FxHashSet<String> = names.iter().cloned().collect();
        let to_hide: Vec<String> = self
            .bindings
            .keys()
            .filter(|k| !allow.contains(*k))
            .cloned()
            .collect();
        self.hidden.extend(to_hide);
    }

    /// Create an extended context with an additional binding.
    pub fn extend(
        &self,
        name: impl Into<String>,
        ty: Type,
        cap: Capability,
        mutable: bool,
    ) -> Self {
        let mut ctx = self.clone();
        ctx.bind(name, ty, cap, mutable);
        ctx
    }

    /// Set the entity event declarations for emit validation.
    pub fn set_entity_events(&mut self, events: Vec<(String, Vec<(String, Type)>)>) {
        self.entity_events = Some(events);
    }

    /// Record that a type variable must satisfy a class constraint.
    /// Multiple constraints on the same variable are accumulated.
    pub fn add_constraint(&mut self, tv: TypeVar, class_name: &str) {
        self.constraints
            .entry(tv)
            .or_default()
            .push(class_name.to_string());
    }

    /// Look up the constraints on a type variable, if any.
    pub fn get_constraints(&self, tv: &TypeVar) -> Option<&Vec<String>> {
        self.constraints.get(tv)
    }

    /// Iterate over all bindings as `(name, (type, capability, mutable))` tuples.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &(Type, Capability, bool))> {
        self.bindings.iter()
    }

    /// Free type variables occurring in any binding in the context.
    pub fn free_vars(&self) -> Vec<TypeVar> {
        let mut vars = vec![];
        for (ty, _, _) in self.bindings.values() {
            vars.extend(ty.free_vars());
        }
        vars.sort_by_key(|v| v.0);
        vars.dedup_by_key(|v| v.0);
        vars
    }
}

// ---------------------------------------------------------------------------
// Source Location
// ---------------------------------------------------------------------------
// Source Map (offset -> line/column resolution)
// ---------------------------------------------------------------------------

use std::cell::RefCell;

thread_local! {
    /// Thread-local source map used by Span::line()/column() to resolve byte
    /// offsets into human-readable positions.  Set once per compilation unit
    /// (by the lexer or test harness) before any Span display.
    static SOURCE_MAP: RefCell<Option<SourceMap>> = RefCell::new(None);
}

/// Maps byte offsets to line:column positions for error reporting.
///
/// Retains the full source text so that error formatters can produce
/// source-code excerpts without re-reading from disk.
#[derive(Debug, Clone)]
pub struct SourceMap {
    /// Byte offset of the start of each line.  line_starts[0] is always 0.
    line_starts: Vec<u32>,
    /// The full source text (retained for source excerpts in error messages).
    source: String,
    /// Optional file path (e.g. "main.nula"); used in `--> file:line:col`.
    file_path: Option<String>,
}

impl SourceMap {
    /// Build a source map from source text.  Line endings are `\n` only.
    pub fn new(source: &str) -> Self {
        Self::with_file(source, None)
    }

    /// Build a source map with an optional file path for richer diagnostics.
    pub fn with_file(source: &str, file_path: Option<&str>) -> Self {
        let mut line_starts = vec![0u32];
        for (i, &b) in source.as_bytes().iter().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        SourceMap {
            line_starts,
            source: source.to_string(),
            file_path: file_path.map(|s| s.to_string()),
        }
    }

    /// Resolve a byte offset to (1-indexed line, 1-indexed column).
    pub fn line_col(&self, offset: u32) -> (usize, usize) {
        let idx = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let line = idx + 1;
        let col = offset.saturating_sub(self.line_starts[idx]) + 1;
        (line, col as usize)
    }

    /// Return the 1-indexed source line (without trailing newline), if in range.
    pub fn source_line(&self, line: usize) -> Option<&str> {
        if line == 0 || line > self.line_starts.len() {
            return None;
        }
        let start = self.line_starts[line - 1] as usize;
        let end = if line < self.line_starts.len() {
            // End before the next line's start, skipping the `\n`.
            (self.line_starts[line] as usize).saturating_sub(1)
        } else {
            self.source.len()
        };
        if start <= end && start <= self.source.len() {
            Some(&self.source[start..end.min(self.source.len())])
        } else {
            None
        }
    }

    /// Return a slice of source text from `offset` for `len` bytes.
    pub fn source_slice(&self, offset: u32, len: u32) -> Option<&str> {
        let start = offset as usize;
        let end = (start + len as usize).min(self.source.len());
        if start < self.source.len() {
            Some(&self.source[start..end])
        } else {
            None
        }
    }

    /// Return the optional file path stored in this map.
    pub fn file_path(&self) -> Option<&str> {
        self.file_path.as_deref()
    }

    /// Return the full source text retained by this map. Used by the rich
    /// diagnostic renderer (`crate::diagnostic`) to build source snippets.
    pub fn source_text(&self) -> &str {
        &self.source
    }
}
/// Install a SourceMap for the current thread, consuming the source string
/// to build line-start offsets.  Call before any Span display.
pub fn set_source_map(source: &str) {
    set_source_map_with_file(source, None);
}

pub fn source_map_file() -> Option<String> {
    SOURCE_MAP.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|sm| sm.file_path().map(|s| s.to_string()))
    })
}

/// Return the full source text from the thread-local SourceMap, if one is
/// installed. Used by the rich diagnostic renderer to build source snippets.
pub fn current_source_text() -> Option<String> {
    SOURCE_MAP.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|sm| sm.source_text().to_string())
    })
}

/// Install a SourceMap with an optional file path (e.g. "main.nula").
/// Call before any Span display for richer diagnostics.
pub fn set_source_map_with_file(source: &str, file: Option<&str>) {
    let sm = SourceMap::with_file(source, file);
    SOURCE_MAP.with(|slot| {
        *slot.borrow_mut() = Some(sm);
    });
}

/// Clear the thread-local source map (e.g. between tests).
pub fn clear_source_map() {
    SOURCE_MAP.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

/// Return the source text for a byte-offset span using the thread-local
/// SourceMap, if one is installed. Returns `None` when no SourceMap is set
/// (e.g. in synthetic contexts) or the span extends beyond the source.
pub fn source_slice_for_span(span: Span) -> Option<String> {
    SOURCE_MAP.with(|slot| {
        slot.borrow().as_ref().and_then(|sm| {
            sm.source_slice(span.start, span.end.saturating_sub(span.start))
                .map(|s| s.to_string())
        })
    })
}

/// Compact source span — just byte offsets.  Line/column are resolved on
/// demand via the thread-local SourceMap (set by the lexer or test harness).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: u32, end: u32) -> Self {
        Span { start, end }
    }

    /// 1-indexed line number (reads from thread-local SourceMap; returns 0
    /// if none is set).
    pub fn line(&self) -> usize {
        SOURCE_MAP.with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|sm| sm.line_col(self.start).0)
                .unwrap_or(0)
        })
    }

    /// 1-indexed column (reads from thread-local SourceMap; returns 0 if
    /// none is set).
    pub fn column(&self) -> usize {
        SOURCE_MAP.with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|sm| sm.line_col(self.start).1)
                .unwrap_or(0)
        })
    }

    /// 1-indexed line number of span end (reads from thread-local SourceMap).
    pub fn end_line(&self) -> usize {
        SOURCE_MAP.with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|sm| sm.line_col(self.end).0)
                .unwrap_or(0)
        })
    }

    /// 1-indexed column of span end (reads from thread-local SourceMap).
    pub fn end_column(&self) -> usize {
        SOURCE_MAP.with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|sm| sm.line_col(self.end).1)
                .unwrap_or(0)
        })
    }

    /// The source line containing this span's start, if a SourceMap is set.
    pub fn source_line(&self) -> Option<String> {
        SOURCE_MAP.with(|slot| {
            slot.borrow().as_ref().and_then(|sm| {
                let (line, _) = sm.line_col(self.start);
                sm.source_line(line).map(|s| s.to_string())
            })
        })
    }

    /// The file path stored in the SourceMap, if any.
    pub fn file(&self) -> Option<String> {
        SOURCE_MAP.with(|slot| {
            slot.borrow()
                .as_ref()
                .and_then(|sm| sm.file_path().map(|s| s.to_string()))
        })
    }
}

// ---------------------------------------------------------------------------
// Nulang Result Type
// ---------------------------------------------------------------------------

pub type NuResult<T> = Result<T, NuError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    E001UnclosedDelimiter,
    E002UnboundVariable,
    E003TypeMismatch,
    E004MissingEffect,
    E005SendabilityViolation,
    E006LinearUseAfterConsume,
    E007InfiniteType,
    E008FieldNotFound,
    E009WrongArity,
    E010MatchNoArms,
    E011StepLimitExceeded,
    E012UnhandledEffect,
    E013FfiBoundaryViolation,
}

impl ErrorCode {
    pub fn code_str(&self) -> &'static str {
        match self {
            ErrorCode::E001UnclosedDelimiter => "E001",
            ErrorCode::E002UnboundVariable => "E002",
            ErrorCode::E003TypeMismatch => "E003",
            ErrorCode::E004MissingEffect => "E004",
            ErrorCode::E005SendabilityViolation => "E005",
            ErrorCode::E006LinearUseAfterConsume => "E006",
            ErrorCode::E007InfiniteType => "E007",
            ErrorCode::E008FieldNotFound => "E008",
            ErrorCode::E009WrongArity => "E009",
            ErrorCode::E010MatchNoArms => "E010",
            ErrorCode::E011StepLimitExceeded => "E011",
            ErrorCode::E012UnhandledEffect => "E012",
            ErrorCode::E013FfiBoundaryViolation => "E013",
        }
    }
    pub fn explain(&self) -> &'static str {
        match self {
            ErrorCode::E001UnclosedDelimiter => "An opening delimiter was never closed.",
            ErrorCode::E002UnboundVariable => "A name was used but never defined in scope.",
            ErrorCode::E003TypeMismatch => "Two types that should be equal are not.",
            ErrorCode::E004MissingEffect => {
                "A function performs an effect not in its declared effect row."
            }
            ErrorCode::E005SendabilityViolation => {
                "A value with an unsafe capability was sent between actors."
            }
            ErrorCode::E006LinearUseAfterConsume => {
                "A linear value was used after it was consumed."
            }
            ErrorCode::E007InfiniteType => "The occurs check failed.",
            ErrorCode::E008FieldNotFound => "A record field was accessed but does not exist.",
            ErrorCode::E009WrongArity => {
                "A function was called with the wrong number of arguments."
            }
            ErrorCode::E010MatchNoArms => "A match expression has zero arms.",
            ErrorCode::E011StepLimitExceeded => "The VM step limit was exceeded.",
            ErrorCode::E012UnhandledEffect => {
                "An effect was performed but no handler exists for it."
            }
            ErrorCode::E013FfiBoundaryViolation => {
                "A capability-qualified VM-heap type was used at an FFI boundary; foreign threads are not tracked by ORCA. Move the value into a serialized form (String) or an externally-managed opaque handle."
            }
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.code_str())
    }
}

/// A compiler or runtime error with structured diagnostic information.
///
/// Each variant carries the core `msg` and `span` plus optional fields
/// that power rich diagnostic output (expected/found types, missing effects,
/// fix suggestions). Construction helpers (e.g. `NuError::type_mismatch`)
/// should be preferred for common patterns.
#[derive(Debug, Clone)]
pub enum NuError {
    LexError {
        msg: String,
        span: Span,
    },
    ParseError {
        msg: String,
        span: Span,
        /// What the parser expected (e.g. `"','"`, `"a pattern"`).
        expected: Option<String>,
        /// What the parser found instead.
        found: Option<String>,
    },
    TypeError {
        msg: String,
        span: Span,
        /// The type that was expected (lhs of unification failure).
        expected_type: Option<String>,
        /// The type that was found (rhs of unification failure).
        found_type: Option<String>,
        /// For unbound-variable errors: list of names in scope that might be
        /// close (e.g. `"foo"` when `"fooo"` was typed).
        similar_names: Option<Vec<String>>,
    },
    EffectError {
        msg: String,
        span: Span,
        /// Effects that are produced but not declared in the function signature.
        missing_effects: Option<Vec<String>>,
        /// Effects currently allowed by the enclosing function annotation.
        allowed_effects: Option<String>,
    },
    CapError {
        msg: String,
        span: Span,
        /// A concise explanation of the capability rule that was violated.
        explanation: Option<String>,
    },
    FFIError {
        msg: String,
        span: Span,
    },
    /// Feature is parsed/typed correctly but has no runtime implementation yet.
    NotYetImplemented {
        feature: String,
        span: Span,
    },
    RuntimeError {
        msg: String,
        span: Span,
    },
    VMError {
        msg: String,
        span: Span,
    },
    /// The VM suspended a behavior (waiting for a signal, LLM completion, or
    /// selective receive). Not a failure — the runtime captures the suspension
    /// state and resumes the behavior later.
    Suspended(VmSuspension),
    PythonError {
        msg: String,
        span: Span,
    },
    PackageError {
        msg: String,
        span: Span,
    },
    /// Multiple errors accumulated during error-recovery parsing.
    /// The `Vec` is non-empty; individual errors preserve their spans.
    Multiple(Vec<NuError>),
}

/// Reason the VM suspended execution of a behavior.
///
/// Carried by [`NuError::Suspended`]; the runtime inspects this to decide
/// what to wait for before resuming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmSuspension {
    /// `perform Signal.wait(name)` — awaiting a workflow signal.
    SignalWait,
    /// `receive { ... } after ms => ...` — awaiting a matching message or timeout.
    ReceiveWait,
    /// `perform <Effect>.<op>(...)` — awaiting an async effect completion.
    PerformAsync,
}

impl std::fmt::Display for VmSuspension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmSuspension::SignalWait => write!(f, "SignalWait"),
            VmSuspension::ReceiveWait => write!(f, "ReceiveWait"),
            VmSuspension::PerformAsync => write!(f, "PerformAsync"),
        }
    }
}

impl std::fmt::Display for NuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NuError::LexError { msg, span } => {
                write!(f, "Lex error at {}:{}: {}", span.line(), span.column(), msg)
            }
            NuError::ParseError {
                msg,
                span,
                expected,
                found,
            } => {
                write!(
                    f,
                    "Parse error at {}:{}: {}",
                    span.line(),
                    span.column(),
                    msg
                )?;
                if let Some(exp) = expected {
                    write!(f, "\n  expected: {exp}")?;
                }
                if let Some(fnd) = found {
                    write!(f, "\n  found: {fnd}")?;
                }
                Ok(())
            }
            NuError::TypeError {
                msg,
                span,
                expected_type,
                found_type,
                similar_names,
            } => {
                write!(
                    f,
                    "Type error at {}:{}: {}",
                    span.line(),
                    span.column(),
                    msg
                )?;
                if let Some(exp) = expected_type {
                    write!(f, "\n  expected type: {exp}")?;
                }
                if let Some(fnd) = found_type {
                    write!(f, "\n  found type: {fnd}")?;
                }
                if let Some(names) = similar_names {
                    if !names.is_empty() {
                        write!(f, "\n  did you mean one of: {}?", names.join(", "))?;
                    }
                }
                Ok(())
            }
            NuError::EffectError {
                msg,
                span,
                missing_effects,
                allowed_effects,
            } => {
                write!(
                    f,
                    "Effect error at {}:{}: {}",
                    span.line(),
                    span.column(),
                    msg
                )?;
                if let Some(missing) = missing_effects {
                    if !missing.is_empty() {
                        write!(f, "\n  missing effects: {}", missing.join(", "))?;
                    }
                }
                if let Some(allowed) = allowed_effects {
                    write!(f, "\n  allowed effects: {allowed}")?;
                }
                Ok(())
            }
            NuError::CapError {
                msg,
                span,
                explanation,
            } => {
                write!(
                    f,
                    "Capability error at {}:{}: {}",
                    span.line(),
                    span.column(),
                    msg
                )?;
                if let Some(expl) = explanation {
                    write!(f, "\n  note: {expl}")?;
                }
                Ok(())
            }
            NuError::FFIError { msg, span } => {
                write!(f, "FFI error at {}:{}: {}", span.line(), span.column(), msg)
            }
            NuError::NotYetImplemented { feature, span } => {
                write!(
                    f,
                    "Not yet implemented at {}:{}: {}",
                    span.line(),
                    span.column(),
                    feature
                )
            }
            NuError::RuntimeError { msg, span } => {
                write!(
                    f,
                    "Runtime error at {}:{}: {}",
                    span.line(),
                    span.column(),
                    msg
                )
            }
            NuError::VMError { msg, span } => {
                write!(f, "VM error at {}:{}: {}", span.line(), span.column(), msg)
            }
            NuError::Suspended(kind) => write!(f, "VM suspended: {}", kind),
            NuError::PythonError { msg, span } => {
                write!(
                    f,
                    "Python error at {}:{}: {}",
                    span.line(),
                    span.column(),
                    msg
                )
            }
            NuError::Multiple(errors) => {
                writeln!(f, "{} parse errors:", errors.len())?;
                for (i, err) in errors.iter().enumerate() {
                    writeln!(f, "  {}. {}", i + 1, err)?;
                }
                Ok(())
            }
            NuError::PackageError { msg, span } => {
                write!(
                    f,
                    "Package error at {}:{}: {}",
                    span.line(),
                    span.column(),
                    msg
                )
            }
        }
    }
}

impl NuError {
    /// Construct a ParseError.
    pub fn parse_error(msg: String, span: Span) -> Self {
        NuError::ParseError {
            msg,
            span,
            expected: None,
            found: None,
        }
    }

    /// Construct a CapError.
    pub fn cap_error(msg: String, span: Span) -> Self {
        NuError::CapError {
            msg,
            span,
            explanation: None,
        }
    }

    /// Construct a CapError with a concise rule explanation rendered as a
    /// `note:` line by `Display`. Prefer this over [`NuError::cap_error`]
    /// when the violated capability rule is known at the call site.
    pub fn cap_error_explained(msg: String, span: Span, explanation: impl Into<String>) -> Self {
        NuError::CapError {
            msg,
            span,
            explanation: Some(explanation.into()),
        }
    }

    /// Construct an EffectError.
    pub fn effect_error(msg: String, span: Span) -> Self {
        NuError::EffectError {
            msg,
            span,
            missing_effects: None,
            allowed_effects: None,
        }
    }

    /// Construct a RuntimeError.
    pub fn runtime_error(msg: String, span: Span) -> Self {
        NuError::RuntimeError { msg, span }
    }

    /// Construct a VMError.
    pub fn vm_error(msg: String, span: Span) -> Self {
        NuError::VMError { msg, span }
    }

    /// Construct an FFIError.
    pub fn ffi_error(msg: String, span: Span) -> Self {
        NuError::FFIError { msg, span }
    }
    /// Produce a colorized, multi-line error message with source excerpts and
    /// carets. Uses ANSI escape codes; callers should gate on `is_terminal()`
    /// if they support plain-text fallback.
    pub fn format_rich(&self) -> String {
        // ANSI helpers
        const RED: &str = "\x1b[1;31m";
        const CYAN: &str = "\x1b[36m";
        const BLUE: &str = "\x1b[34m";
        const BOLD: &str = "\x1b[1m";
        const RESET: &str = "\x1b[0m";
        const DIM: &str = "\x1b[2m";
        const GREEN: &str = "\x1b[32m";
        const YELLOW: &str = "\x1b[33m";

        let mut out = String::new();

        /// Push a spanned error header + source excerpt + caret into `out`.
        fn push_span_error(
            out: &mut String,
            kind: &str,
            msg: &str,
            span: &Span,
            code: Option<&str>,
            suggestion: Option<&str>,
            extra_lines: &[String],
        ) {
            let line = span.line();
            let col = span.column();
            let file = span.file().unwrap_or_default();

            // Header with error code
            write_header(out, kind, line, col, &file, code);

            // Source excerpt
            if let Some(src_line) = span.source_line() {
                let col_idx = col.saturating_sub(1).min(src_line.len());
                let span_len = (span.end.saturating_sub(span.start) as usize)
                    .max(1)
                    .min(src_line.len().saturating_sub(col_idx));

                let line_label = format!("{line}");
                out.push_str(&format!("{DIM}{line_label:>4} {BLUE}|{RESET} "));
                out.push_str(&src_line[..col_idx]);
                out.push_str(RED);
                let end = (col_idx + span_len).min(src_line.len());
                out.push_str(&src_line[col_idx..end]);
                out.push_str(RESET);
                out.push_str(&src_line[end..]);
                out.push('\n');

                // Caret line
                let padding = format!("{:>4} {BLUE}|{RESET} ", "");
                out.push_str(&padding);
                for _ in 0..col_idx {
                    out.push(' ');
                }
                out.push_str(RED);
                for _ in 0..span_len {
                    out.push('^');
                }
                out.push(' ');
                out.push_str(msg);
                out.push_str(RESET);
                out.push('\n');
            } else {
                out.push_str(&format!("  {msg}\n"));
            }

            // Extra diagnostic lines (expected/found types, missing effects, etc.)
            for extra in extra_lines {
                let padding = format!("{:>4} {BLUE}|{RESET} ", "");
                out.push_str(&padding);
                out.push_str(extra);
                out.push('\n');
            }

            // Suggestion
            if let Some(s) = suggestion {
                out.push_str(&format!("{BLUE}help:{RESET} {s}\n"));
            }
        }

        fn write_header(
            out: &mut String,
            kind: &str,
            line: usize,
            col: usize,
            file: &str,
            code: Option<&str>,
        ) {
            let code_str = code.map(|c| format!("[{c}]")).unwrap_or_default();
            out.push_str(&format!(
                "{RED}error{code_str}{RESET}{BOLD}: {kind}{RESET}\n"
            ));
            if file.is_empty() {
                out.push_str(&format!("  {BLUE}--> {RESET}{line}:{col}\n"));
            } else {
                out.push_str(&format!("  {BLUE}--> {RESET}{file}:{line}:{col}\n"));
            }
            out.push_str(&format!(" {DIM}{line:>4} {CYAN}|{RESET}\n"));
        }

        let code = self.stable_code();

        match self {
            NuError::LexError { msg, span } => {
                push_span_error(
                    &mut out,
                    "Lex error",
                    msg,
                    span,
                    code,
                    self.suggestion(),
                    &[],
                );
            }
            NuError::ParseError {
                msg,
                span,
                expected,
                found,
            } => {
                let mut extras = Vec::new();
                if let Some(exp) = expected {
                    extras.push(format!("{GREEN}expected:{RESET} {exp}"));
                }
                if let Some(fnd) = found {
                    extras.push(format!("{YELLOW}found:{RESET} {fnd}"));
                }
                push_span_error(
                    &mut out,
                    "Parse error",
                    msg,
                    span,
                    code,
                    self.suggestion(),
                    &extras,
                );
            }
            NuError::TypeError {
                msg,
                span,
                expected_type,
                found_type,
                similar_names,
            } => {
                let mut extras = Vec::new();
                if let Some(exp) = expected_type {
                    extras.push(format!("{GREEN}expected type:{RESET} {exp}"));
                }
                if let Some(fnd) = found_type {
                    extras.push(format!("{YELLOW}found type:{RESET} {fnd}"));
                }
                if let Some(names) = similar_names {
                    if !names.is_empty() {
                        extras.push(format!("{BLUE}did you mean:{RESET} {}?", names.join(", ")));
                    }
                }
                push_span_error(
                    &mut out,
                    "Type error",
                    msg,
                    span,
                    code,
                    self.suggestion(),
                    &extras,
                );
            }
            NuError::EffectError {
                msg,
                span,
                missing_effects,
                allowed_effects,
            } => {
                let mut extras = Vec::new();
                if let Some(missing) = missing_effects {
                    if !missing.is_empty() {
                        extras.push(format!(
                            "{RED}missing effects:{RESET} {}",
                            missing.join(", ")
                        ));
                    }
                }
                if let Some(allowed) = allowed_effects {
                    extras.push(format!("{GREEN}allowed effects:{RESET} {allowed}"));
                }
                push_span_error(
                    &mut out,
                    "Effect error",
                    msg,
                    span,
                    code,
                    self.suggestion(),
                    &extras,
                );
            }
            NuError::CapError {
                msg,
                span,
                explanation,
            } => {
                let mut extras = Vec::new();
                if let Some(expl) = explanation {
                    extras.push(format!("{BLUE}note:{RESET} {expl}"));
                }
                push_span_error(
                    &mut out,
                    "Capability error",
                    msg,
                    span,
                    code,
                    self.suggestion(),
                    &extras,
                );
            }
            NuError::FFIError { msg, span } => {
                push_span_error(
                    &mut out,
                    "FFI error",
                    msg,
                    span,
                    code,
                    self.suggestion(),
                    &[],
                );
            }
            NuError::NotYetImplemented { feature, span } => {
                push_span_error(
                    &mut out,
                    "Not yet implemented",
                    feature,
                    span,
                    code,
                    self.suggestion(),
                    &[],
                );
            }
            NuError::RuntimeError { msg, span } => {
                push_span_error(
                    &mut out,
                    "Runtime error",
                    msg,
                    span,
                    code,
                    self.suggestion(),
                    &[],
                );
            }
            NuError::VMError { msg, span } => {
                push_span_error(
                    &mut out,
                    "VM error",
                    msg,
                    span,
                    code,
                    self.suggestion(),
                    &[],
                );
            }
            NuError::Suspended(kind) => {
                out.push_str(&format!(
                    "{BLUE}info{RESET}{BOLD}: VM suspended ({kind}){RESET}\n"
                ));
            }
            NuError::PythonError { msg, span } => {
                push_span_error(
                    &mut out,
                    "Python error",
                    msg,
                    span,
                    code,
                    self.suggestion(),
                    &[],
                );
            }
            NuError::PackageError { msg, span } => {
                push_span_error(
                    &mut out,
                    "Package error",
                    msg,
                    span,
                    code,
                    self.suggestion(),
                    &[],
                );
            }
            NuError::Multiple(errors) => {
                for err in errors {
                    out.push_str(&err.format_rich());
                    out.push('\n');
                }
            }
        }
        out
    }

    /// Return the canonical error code for this error, preferring structured
    /// fields over message-pattern heuristics.
    pub fn error_code(&self) -> Option<ErrorCode> {
        // FFI foreign-heap boundary violations are TypeErrors with a
        // dedicated stable code (E0208); classify them before the generic
        // structured-field and message heuristics below.
        if matches!(self, NuError::TypeError { .. })
            && self.msg_str().contains("cannot cross the FFI boundary")
        {
            return Some(ErrorCode::E013FfiBoundaryViolation);
        }
        // Structured-field shortcuts (more reliable than string matching).
        match self {
            NuError::TypeError {
                expected_type: Some(_),
                found_type: Some(_),
                ..
            } => {
                return Some(ErrorCode::E003TypeMismatch);
            }
            NuError::TypeError {
                similar_names: Some(_),
                ..
            } => {
                let msg = self.msg_str();
                if msg.contains("Unbound variable") {
                    return Some(ErrorCode::E002UnboundVariable);
                }
            }
            NuError::EffectError {
                missing_effects: Some(_),
                ..
            } => {
                return Some(ErrorCode::E004MissingEffect);
            }
            NuError::CapError {
                explanation: Some(_),
                ..
            } => {
                let msg = self.msg_str();
                if msg.contains("cannot be sent") || msg.contains("sendable") {
                    return Some(ErrorCode::E005SendabilityViolation);
                }
                if msg.contains("linear") && msg.contains("consumed") {
                    return Some(ErrorCode::E006LinearUseAfterConsume);
                }
            }
            _ => {}
        }

        // Fall back to message-string heuristics.
        let msg = self.msg_str();
        if msg.is_empty() {
            return None;
        }
        if msg.contains("unclosed") {
            Some(ErrorCode::E001UnclosedDelimiter)
        } else if msg.contains("Unbound variable") {
            Some(ErrorCode::E002UnboundVariable)
        } else if msg.contains("Cannot unify") || msg.contains("Type mismatch") {
            Some(ErrorCode::E003TypeMismatch)
        } else if msg.contains("not a subset of allowed effects")
            || msg.contains("contain disallowed effect")
        {
            Some(ErrorCode::E004MissingEffect)
        } else if msg.contains("cannot be sent") {
            Some(ErrorCode::E005SendabilityViolation)
        } else if msg.contains("linear") && msg.contains("consumed") {
            Some(ErrorCode::E006LinearUseAfterConsume)
        } else if msg.contains("Infinite type") {
            Some(ErrorCode::E007InfiniteType)
        } else if msg.contains("Field") && msg.contains("not found") {
            Some(ErrorCode::E008FieldNotFound)
        } else if msg.contains("wrong number of arguments") {
            Some(ErrorCode::E009WrongArity)
        } else if msg.contains("Match expression with no arms") {
            Some(ErrorCode::E010MatchNoArms)
        } else if msg.contains("Step limit exceeded") {
            Some(ErrorCode::E011StepLimitExceeded)
        } else if msg.contains("Unhandled effect") {
            Some(ErrorCode::E012UnhandledEffect)
        } else {
            None
        }
    }

    /// Extract the primary message string from any error variant.
    fn msg_str(&self) -> &str {
        match self {
            NuError::LexError { msg, .. } => msg,
            NuError::ParseError { msg, .. } => msg,
            NuError::TypeError { msg, .. } => msg,
            NuError::EffectError { msg, .. } => msg,
            NuError::CapError { msg, .. } => msg,
            NuError::FFIError { msg, .. } => msg,
            NuError::RuntimeError { msg, .. } => msg,
            NuError::VMError { msg, .. } => msg,
            NuError::PythonError { msg, .. } => msg,
            NuError::PackageError { msg, .. } => msg,
            NuError::NotYetImplemented { feature, .. } => feature,
            NuError::Multiple(_) => "",
            NuError::Suspended(_) => "",
        }
    }

    /// Return a fix suggestion for this error, using both structured fields
    /// and message-pattern heuristics.
    pub fn suggestion(&self) -> Option<&str> {
        match self {
            NuError::ParseError {
                msg,
                expected,
                found,
                ..
            } => {
                if msg.starts_with("Expected 'fn'") {
                    Some("did you mean `fn`?")
                } else if msg.contains("unclosed") || msg.starts_with("Expected '}'") {
                    Some("unclosed delimiter — check that every `{`, `(`, and `[` has a matching close")
                } else if msg.contains("Expected ')'") {
                    Some("unclosed parenthesis — add a `)` to close it")
                } else if msg.contains("Expected '('") {
                    Some("missing opening parenthesis — add `(` before the arguments")
                } else if msg.contains("Expected ']'") {
                    Some("unclosed bracket — add a `]` to close the list or array type")
                } else if msg.contains("Expected identifier") || msg.contains("Expected variable") {
                    Some("expected a name here — variable names must start with a letter or underscore")
                } else if msg.contains("Expected integer") {
                    Some("expected an integer literal like `42` or `0xFF`")
                } else if msg.contains("Expected float") {
                    Some("expected a float literal like `3.14` or `1.0`")
                } else if msg.contains("Expected string") {
                    Some("expected a string literal in quotes like \"hello\"")
                } else if msg.contains("Expected type") {
                    Some(
                        "expected a type name like `Int`, `String`, `Bool`, or a user-defined type",
                    )
                } else if msg.contains("Expected pattern") {
                    Some(
                        "expected a pattern — try a variable name, literal, or constructor pattern",
                    )
                } else if msg.contains("Expected ';'") || msg.contains("Expected line break") {
                    Some("each statement must end with a newline or `;`")
                } else if msg.contains("Expected '{'") || msg.contains("Expected '}'") {
                    Some("expected a block delimited by `{ ... }`")
                } else if msg.contains("Unexpected end of file") {
                    Some("the source file ended before the expression or declaration was complete")
                } else if msg.contains("Expected '=>'") {
                    Some("match arms use `=>` between the pattern and body, like `case 1 => body`")
                } else if let (Some(_), Some(_)) = (expected, found) {
                    // Generic expected/found suggestion
                    None
                } else {
                    None
                }
            }
            NuError::TypeError {
                msg,
                expected_type,
                similar_names,
                ..
            } => {
                if msg.contains("Unbound variable") {
                    if let Some(names) = similar_names {
                        if !names.is_empty() {
                            return None; // "did you mean" already shown in the main output
                        }
                    }
                    Some("this name is not defined in the current scope — check for typos or missing definitions")
                } else if msg.contains("Cannot unify") && msg.contains("record") {
                    Some("check that all required fields are present and have the correct types")
                } else if msg.contains("Cannot unify Int") && msg.contains("Float") {
                    Some("Int and Float are different types; convert with `.to_float()` or `.to_int()`")
                } else if msg.contains("Cannot unify") && msg.contains("function") {
                    Some("function type mismatch — check that the argument types and return type match")
                } else if msg.contains("Cannot unify") || msg.contains("Type mismatch") {
                    if let (Some(exp), _) = (expected_type, &None::<String>) {
                        if exp.contains("Int") {
                            return Some("the expression produces the wrong type — consider adding a type annotation or conversion");
                        }
                    }
                    Some("the types do not match — check the expression and add type annotations if needed")
                } else if msg.contains("Infinite type") {
                    Some("this means a value references itself in a cycle — try restructuring the definition")
                } else if msg.contains("Field") && msg.contains("not found") {
                    Some("the record does not have this field — check the spelling or use the correct record type")
                } else if msg.contains("Unsupported FFI type") {
                    Some(
                        "only Int, Float, Bool, String, and Unit are supported in FFI declarations",
                    )
                } else if msg.contains("Match expression with no arms") {
                    Some("add at least one pattern match arm, e.g. `case pattern => expression`")
                } else if msg.contains("wrong number of arguments") {
                    Some("the function expects a different number of arguments — check the definition")
                } else if msg.contains("Unknown event") {
                    Some("check the event name spelling against the available events listed above")
                } else {
                    None
                }
            }
            NuError::EffectError {
                msg,
                missing_effects,
                ..
            } => {
                if let Some(missing) = missing_effects {
                    if !missing.is_empty() {
                        return Some("add the missing effects to the function's effect annotation, or wrap the call in a handler block");
                    }
                }
                if msg.contains("not a subset of allowed effects") {
                    Some("add the missing effects to the enclosing function's effect annotation, or use a handler")
                } else if msg.contains("Unhandled effect") {
                    Some("the effect is not handled anywhere in the call stack — add a `with handler { ... }` block or declare it in the function signature")
                } else {
                    None
                }
            }
            NuError::CapError { msg, .. } => {
                if msg.contains("cannot be sent")
                    || msg.contains("sendable")
                    || msg.contains("send argument")
                {
                    Some("only `val`, `iso`, `tag`, and `linear` capabilities are sendable between actors — use `val` for immutable shared data, `iso` for transfer-only ownership")
                } else if msg.contains("linear") && msg.contains("consumed") {
                    Some("linear values can only be used once — use `.clone()` to make a copy, or restructure to avoid the second use")
                } else if msg.contains("downgrade") {
                    Some("capability downgrade is not allowed here — the value must keep its current or stronger capability")
                } else if msg.contains("Not a subtype") {
                    Some("this capability is not compatible with the expected one — check the subtyping rules: iso < trn < ref < box, linear < val < box")
                } else {
                    None
                }
            }
            NuError::RuntimeError { msg, .. } => {
                if msg.contains("Stack overflow") {
                    Some("the recursion depth is too high — check for unbounded recursion or increase the stack limit")
                } else if msg.contains("Step limit exceeded") {
                    Some("the computation took too many steps — check for infinite loops")
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    // -----------------------------------------------------------------------
    // Constructor helpers for common error patterns
    // -----------------------------------------------------------------------

    /// Create a type-mismatch error with explicit expected/found types.
    /// Construct a TypeError with a simple message.
    pub fn type_error(msg: String, span: Span) -> Self {
        NuError::TypeError {
            msg,
            span,
            expected_type: None,
            found_type: None,
            similar_names: None,
        }
    }

    pub fn type_mismatch(
        expected: impl Into<String>,
        found: impl Into<String>,
        span: Span,
    ) -> Self {
        let exp = expected.into();
        let fnd = found.into();
        NuError::TypeError {
            msg: format!("Type mismatch: expected {}, found {}", exp, fnd),
            span,
            expected_type: Some(exp),
            found_type: Some(fnd),
            similar_names: None,
        }
    }

    /// Create a type error for an unbound (undefined) variable.
    pub fn unbound_variable(
        name: impl Into<String>,
        span: Span,
        in_scope: Option<Vec<String>>,
    ) -> Self {
        let name = name.into();
        let similar = in_scope.as_ref().and_then(|names| {
            // Find names within edit distance ≤ 2.
            let close: Vec<String> = names
                .iter()
                .filter(|n| levenshtein(&name, n) <= 2 && n.as_str() != &name)
                .take(5)
                .cloned()
                .collect();
            if close.is_empty() {
                None
            } else {
                Some(close)
            }
        });
        NuError::TypeError {
            msg: format!("Unbound variable: '{}'", name),
            span,
            expected_type: None,
            found_type: None,
            similar_names: similar,
        }
    }

    /// Create a field-not-found error for record access.
    pub fn field_not_found(
        field: impl Into<String>,
        span: Span,
        available: Option<Vec<String>>,
    ) -> Self {
        let field = field.into();
        let msg = if let Some(ref fields) = available {
            format!(
                "Field '{}' not found in record type. Available fields: {}",
                field,
                fields.join(", ")
            )
        } else {
            format!("Field '{}' not found in record type", field)
        };
        NuError::TypeError {
            msg,
            span,
            expected_type: None,
            found_type: None,
            similar_names: None,
        }
    }

    /// Create an effect error listing which effects are missing.
    pub fn missing_effects(missing: Vec<String>, allowed: impl Into<String>, span: Span) -> Self {
        let allowed = allowed.into();
        let msg = format!(
            "effects contain disallowed effect(s): {} (allowed: {})",
            missing.join(", "),
            allowed
        );
        NuError::EffectError {
            msg,
            span,
            missing_effects: Some(missing),
            allowed_effects: Some(allowed),
        }
    }

    /// Create a parsing error for an unexpected token.
    pub fn parse_unexpected(
        expected: impl Into<String>,
        found: impl Into<String>,
        span: Span,
    ) -> Self {
        let exp = expected.into();
        let fnd = found.into();
        NuError::ParseError {
            msg: format!("Expected {}, found {}", exp, fnd),
            span,
            expected: Some(exp),
            found: Some(fnd),
        }
    }
}

/// Compute the Levenshtein (edit) distance between two strings.
/// Used for "did you mean?" suggestions on unbound variables.
fn levenshtein(a: &str, b: &str) -> usize {
    let a_len = a.chars().count();
    let b_len = b.chars().count();
    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr = vec![0usize; b_len + 1];

    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.chars().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_len]
}

impl std::error::Error for NuError {}

// ---------------------------------------------------------------------------
// Exit Reason (Actor Lifecycle)
// ---------------------------------------------------------------------------

/// Reason for an actor's exit, modeled after Erlang's exit reasons.
///
/// Used with link/monitor signal propagation and supervision decisions.
/// The `Shutdown` variant carries an optional timeout for graceful shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// Normal termination — no supervisor notification, linked actors unaffected.
    Normal,
    /// Unconditional kill — cannot be trapped, always triggers cascading exit.
    Kill,
    /// Actor was killed by another actor (the `Kill` reason after propagation).
    Killed,
    /// Graceful shutdown with optional timeout.
    Shutdown(Option<Duration>),
    /// Error with description.
    Error(String),
    /// The node hosting the target actor was declared failed (Erlang's
    /// `noconnection`): the target may still be alive, but is unreachable.
    NoConnection,
    /// User-defined exit reason (any serializable value).
    Custom(String),
}

impl ExitReason {
    /// Returns true if this reason represents abnormal termination.
    ///
    /// Normal exits do NOT trigger linked actor exits (per Erlang semantics).
    /// All other reasons trigger cascading failure for linked actors
    /// that don't trap exits.
    pub fn is_abnormal(&self) -> bool {
        !matches!(self, ExitReason::Normal)
    }

    /// Returns a short string tag for logging/monitoring.
    pub fn tag(&self) -> &'static str {
        match self {
            ExitReason::Normal => "normal",
            ExitReason::Kill => "kill",
            ExitReason::Killed => "killed",
            ExitReason::Shutdown(_) => "shutdown",
            ExitReason::Error(_) => "error",
            ExitReason::NoConnection => "noconnection",
            ExitReason::Custom(_) => "custom",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Structured error diagnostics (PLAN.md Phase 1 bullet 7): verify
    // the NuError constructor helpers actually populate their
    // expected/found/suggestion fields, not just the free-text `msg`.
    // These are unit-level companions to the end-to-end
    // `structerr_*` conformance cases, which verify the same
    // populated fields reach the real error output through
    // `format_rich`/`Display`.
    // -----------------------------------------------------------------

    #[test]
    fn test_type_mismatch_populates_expected_and_found() {
        let err = NuError::type_mismatch("Int", "String", Span::default());
        match &err {
            NuError::TypeError {
                expected_type,
                found_type,
                ..
            } => {
                assert_eq!(expected_type.as_deref(), Some("Int"));
                assert_eq!(found_type.as_deref(), Some("String"));
            }
            other => panic!("expected TypeError, got {other:?}"),
        }
        let displayed = format!("{err}");
        assert!(displayed.contains("expected: Int") || displayed.contains("Int"));
        assert!(displayed.contains("String"));
    }

    #[test]
    fn test_unbound_variable_suggests_close_name_within_edit_distance() {
        let in_scope = vec!["counter".to_string(), "unrelated_far_name".to_string()];
        let err = NuError::unbound_variable("countr", Span::default(), Some(in_scope));
        match &err {
            NuError::TypeError { similar_names, .. } => {
                let names = similar_names.as_ref().expect("expected a suggestion");
                assert_eq!(names, &vec!["counter".to_string()]);
            }
            other => panic!("expected TypeError, got {other:?}"),
        }
        assert!(format!("{err}").contains("did you mean"));
    }

    #[test]
    fn test_unbound_variable_no_suggestion_when_nothing_close() {
        let in_scope = vec!["totally_unrelated".to_string()];
        let err = NuError::unbound_variable("xyz", Span::default(), Some(in_scope));
        match &err {
            NuError::TypeError { similar_names, .. } => {
                assert!(similar_names.is_none());
            }
            other => panic!("expected TypeError, got {other:?}"),
        }
        assert!(!format!("{err}").contains("did you mean"));
    }

    #[test]
    fn test_unbound_variable_no_suggestion_without_scope_context() {
        // Call sites that can't cheaply compute in-scope names (or choose
        // not to) pass `None` — must degrade cleanly, not panic.
        let err = NuError::unbound_variable("whatever", Span::default(), None);
        match &err {
            NuError::TypeError { similar_names, .. } => assert!(similar_names.is_none()),
            other => panic!("expected TypeError, got {other:?}"),
        }
    }

    #[test]
    fn test_missing_effects_populates_structured_fields() {
        let err = NuError::missing_effects(vec!["IO".to_string()], "{}", Span::default());
        match &err {
            NuError::EffectError {
                missing_effects,
                allowed_effects,
                ..
            } => {
                assert_eq!(missing_effects.as_deref(), Some(&["IO".to_string()][..]));
                assert_eq!(allowed_effects.as_deref(), Some("{}"));
            }
            other => panic!("expected EffectError, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_unexpected_populates_expected_and_found() {
        let err = NuError::parse_unexpected("','", "'}'", Span::default());
        match &err {
            NuError::ParseError {
                expected, found, ..
            } => {
                assert_eq!(expected.as_deref(), Some("','"));
                assert_eq!(found.as_deref(), Some("'}'"));
            }
            other => panic!("expected ParseError, got {other:?}"),
        }
        let displayed = format!("{err}");
        assert!(displayed.contains("expected: ','"));
        assert!(displayed.contains("found: '}'"));
    }

    #[test]
    fn test_discharge_linear() {
        assert_eq!(Capability::LinearIso.discharge_linear(), Capability::Iso);
        assert_eq!(Capability::Linear.discharge_linear(), Capability::Val);
        assert_eq!(Capability::Iso.discharge_linear(), Capability::Iso);
        assert_eq!(Capability::Val.discharge_linear(), Capability::Val);
    }

    #[test]
    fn test_linear_join_self() {
        assert_eq!(
            Capability::Linear.join(Capability::Linear),
            Capability::Linear
        );
    }

    #[test]
    fn test_linear_join_val() {
        assert_eq!(Capability::Linear.join(Capability::Val), Capability::Val);
        assert_eq!(Capability::Val.join(Capability::Linear), Capability::Val);
    }

    #[test]
    fn test_linear_join_linearioso() {
        assert_eq!(
            Capability::Linear.join(Capability::LinearIso),
            Capability::Val
        );
        assert_eq!(
            Capability::LinearIso.join(Capability::Linear),
            Capability::Val
        );
    }

    #[test]
    fn test_linear_join_iso() {
        assert_eq!(Capability::Linear.join(Capability::Iso), Capability::Val);
        assert_eq!(Capability::Iso.join(Capability::Linear), Capability::Val);
    }

    #[test]
    fn test_linear_join_trn() {
        assert_eq!(Capability::Linear.join(Capability::Trn), Capability::Val);
        assert_eq!(Capability::Trn.join(Capability::Linear), Capability::Val);
    }

    #[test]
    fn test_linear_join_ref() {
        assert_eq!(Capability::Linear.join(Capability::Ref), Capability::Box);
        assert_eq!(Capability::Ref.join(Capability::Linear), Capability::Box);
    }

    #[test]
    fn test_linear_join_box() {
        assert_eq!(Capability::Linear.join(Capability::Box), Capability::Box);
        assert_eq!(Capability::Box.join(Capability::Linear), Capability::Box);
    }

    #[test]
    fn test_linear_join_tag() {
        assert_eq!(Capability::Linear.join(Capability::Tag), Capability::Linear);
        assert_eq!(Capability::Tag.join(Capability::Linear), Capability::Linear);
    }

    #[test]
    fn test_linear_is_sendable() {
        assert!(Capability::Linear.is_sendable());
    }

    #[test]
    fn test_linear_is_remote_sendable() {
        assert!(Capability::Linear.is_remote_sendable());
        assert!(!Capability::Iso.is_remote_sendable());
        assert!(!Capability::LinearIso.is_remote_sendable());
    }

    #[test]
    fn test_linear_is_not_writable() {
        assert!(!Capability::Linear.is_writable());
    }

    #[test]
    fn test_linear_is_readable() {
        assert!(Capability::Linear.is_readable());
    }

    #[test]
    fn test_linear_is_linear() {
        assert!(Capability::Linear.is_linear());
        assert!(Capability::LinearIso.is_linear());
        assert!(!Capability::Iso.is_linear());
    }

    #[test]
    fn test_linear_subtype_of_val() {
        assert!(Capability::Linear.is_subtype_of(Capability::Val));
    }
}
