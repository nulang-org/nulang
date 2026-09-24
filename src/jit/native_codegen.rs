//! Native machine-code backend boundary for tiered JIT execution.
//!
//! `JitSession` decides *when* to compile and `RegionPlanner` decides *what*
//! is safe to compile. This module owns *how* a planned region becomes native
//! code. The default implementation is Cranelift, but the request/trait shape
//! is intentionally independent of Cranelift IR.

use std::collections::HashMap;

use cranelift::prelude::*;
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::Module;

use crate::bytecode::Instruction;

use super::simd_analyzer::SimdRegion;
use super::typed_compiler::TypeMetadata;
use super::{compiler, simd_compiler, typed_compiler, CodegenOptimization};

/// Specialization requested from a native code generator.
#[derive(Clone, Copy)]
pub(crate) enum NativeCompileKind<'a> {
    /// Generic NaN-tag-aware scalar lowering.
    Scalar {
        native_calls: &'a HashMap<usize, usize>,
    },
    /// Type-directed lowering with guard stripping.
    Typed {
        type_metadata: Option<&'a TypeMetadata>,
    },
    /// Vector lowering for an already-validated SIMD region.
    Simd {
        region: &'a SimdRegion,
    },
}

/// Complete backend request for one native region.
#[derive(Clone, Copy)]
pub(crate) struct NativeCompileRequest<'a> {
    pub(crate) symbol: &'a str,
    pub(crate) start_offset: usize,
    pub(crate) num_instrs: usize,
    pub(crate) instructions: &'a [Instruction],
    pub(crate) optimization: CodegenOptimization,
    pub(crate) kind: NativeCompileKind<'a>,
}

/// A machine-code emitter for planned Nulang bytecode regions.
///
/// The tiering/cache layer deliberately receives only a raw entry pointer.
/// Ownership of executable memory remains with the backend instance for its
/// full lifetime.
pub(crate) trait NativeCodegenBackend {
    fn compile(&mut self, request: NativeCompileRequest<'_>) -> Result<*const u8, String>;
}

/// Current native backend: two Cranelift modules with different latency/quality
/// policies. Keeping them here prevents Cranelift lifecycle state from leaking
/// into `JitSession`.
pub(crate) struct CraneliftCodegen {
    /// Low-latency module used for first native compilation.
    pub(crate) baseline_module: JITModule,
    /// Higher-quality module used after a region proves hot.
    pub(crate) optimized_module: JITModule,
    pub(crate) baseline_builder_context: FunctionBuilderContext,
    pub(crate) baseline_ctx: codegen::Context,
    pub(crate) optimized_builder_context: FunctionBuilderContext,
    pub(crate) optimized_ctx: codegen::Context,
}

impl CraneliftCodegen {
    pub(crate) fn new() -> Option<Self> {
        let baseline_module = Self::new_module("none", "single_pass")?;
        let optimized_module = Self::new_module("speed", "backtracking")?;
        let baseline_ctx = baseline_module.make_context();
        let optimized_ctx = optimized_module.make_context();

        Some(Self {
            baseline_module,
            optimized_module,
            baseline_builder_context: FunctionBuilderContext::new(),
            baseline_ctx,
            optimized_builder_context: FunctionBuilderContext::new(),
            optimized_ctx,
        })
    }

    fn new_module(opt_level: &str, regalloc_algorithm: &str) -> Option<JITModule> {
        let mut flag_builder = settings::builder();
        let _ = flag_builder.set("enable_simd", "true");
        if let Err(e) = flag_builder.set("opt_level", opt_level) {
            eprintln!(
                "JIT: invalid Cranelift opt_level '{}': {} — JIT disabled",
                opt_level, e
            );
            return None;
        }
        if let Err(e) = flag_builder.set("regalloc_algorithm", regalloc_algorithm) {
            eprintln!(
                "JIT: invalid Cranelift regalloc_algorithm '{}': {} — JIT disabled",
                regalloc_algorithm, e
            );
            return None;
        }

        let isa_builder = match cranelift_native::builder() {
            Ok(builder) => builder,
            Err(msg) => {
                eprintln!("JIT: host machine is not supported: {} — JIT disabled", msg);
                return None;
            }
        };
        let isa = match isa_builder.finish(settings::Flags::new(flag_builder)) {
            Ok(isa) => isa,
            Err(e) => {
                eprintln!(
                    "JIT: failed to finalize Cranelift ISA: {} — JIT disabled",
                    e
                );
                return None;
            }
        };

        let mut builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        super::helpers::register_with_builder(&mut builder);
        Some(JITModule::new(builder))
    }

    fn compile_with(
        module: &mut JITModule,
        builder_context: &mut FunctionBuilderContext,
        ctx: &mut codegen::Context,
        request: NativeCompileRequest<'_>,
    ) -> Result<*const u8, String> {
        match request.kind {
            NativeCompileKind::Scalar { native_calls } => compiler::compile_bytecode_region(
                module,
                builder_context,
                ctx,
                request.symbol,
                request.start_offset,
                request.num_instrs,
                request.instructions,
                native_calls,
            )
            .map_err(|e| format!("{e:?}")),
            NativeCompileKind::Typed { type_metadata } => {
                typed_compiler::compile_bytecode_region_typed(
                    module,
                    builder_context,
                    ctx,
                    request.symbol,
                    request.start_offset,
                    request.num_instrs,
                    request.instructions,
                    type_metadata,
                )
                .map_err(|e| format!("{e:?}"))
            }
            NativeCompileKind::Simd { region } => simd_compiler::compile_simd_region(
                module,
                builder_context,
                ctx,
                request.symbol,
                request.instructions,
                region,
            )
            .map_err(|e| format!("{e:?}")),
        }
    }
}

impl NativeCodegenBackend for CraneliftCodegen {
    fn compile(&mut self, request: NativeCompileRequest<'_>) -> Result<*const u8, String> {
        match request.optimization {
            CodegenOptimization::Fast => Self::compile_with(
                &mut self.baseline_module,
                &mut self.baseline_builder_context,
                &mut self.baseline_ctx,
                request,
            ),
            CodegenOptimization::Optimized => Self::compile_with(
                &mut self.optimized_module,
                &mut self.optimized_builder_context,
                &mut self.optimized_ctx,
                request,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{Instruction, OpCode};

    #[test]
    fn test_cranelift_codegen_compiles_fast_and_optimized_scalar_requests() {
        let mut codegen = CraneliftCodegen::new().expect("native Cranelift backend");
        let instructions = vec![
            Instruction::new1(OpCode::Const0, 0),
            Instruction::new3(OpCode::IAdd, 0, 0, 0),
        ];
        let native_calls = HashMap::new();

        for (symbol, optimization) in [
            ("codegen_fast", CodegenOptimization::Fast),
            ("codegen_optimized", CodegenOptimization::Optimized),
        ] {
            let ptr = codegen
                .compile(NativeCompileRequest {
                    symbol,
                    start_offset: 0,
                    num_instrs: instructions.len(),
                    instructions: &instructions,
                    optimization,
                    kind: NativeCompileKind::Scalar {
                        native_calls: &native_calls,
                    },
                })
                .expect("scalar request should compile");
            assert!(!ptr.is_null());
        }
    }
}
