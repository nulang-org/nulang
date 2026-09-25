//! Relocatable native object emission for Nulang MIR.
//!
//! This is intentionally an artifact producer, not a linker. Runtime helpers
//! remain undefined imports in the resulting object and are resolved by a
//! future linker/runtime packaging step.

use cranelift::prelude::*;
use cranelift_frontend::FunctionBuilderContext;
use cranelift_module::{Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};

use crate::mir;
use crate::types::{NuResult, Span};

use super::codegen;

/// A relocatable native object plus the metadata needed by a later linker.
pub struct NativeObjectArtifact {
    bytes: Vec<u8>,
    target: String,
    entry_symbol: Option<String>,
    constants: Vec<crate::bytecode::Constant>,
}

impl NativeObjectArtifact {
    /// Compile a MIR module into a relocatable native object.
    ///
    /// Runtime helper functions are left as undefined imports. Actor behavior
    /// exports are intentionally not part of this first artifact slice; actor
    /// modules fail closed until the stable actor-entry ABI is exported too.
    pub fn compile(mir_module: &mir::Module, target: &str) -> NuResult<Self> {
        if !mir_module.behaviors.is_empty() {
            return Err(crate::types::NuError::VMError {
                msg: "native object emission does not yet export actor behavior entries".into(),
                span: Span::default(),
            });
        }

        let mut flag_builder = settings::builder();
        let _ = flag_builder.set("enable_simd", "true");
        let _ = flag_builder.set("opt_level", "speed");
        let isa_builder = super::create_isa_builder(target)?;
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .map_err(|e| crate::types::NuError::VMError {
                msg: format!(
                    "failed to finalize object ISA for target '{}': {}",
                    target, e
                ),
                span: Span::default(),
            })?;

        let object_builder = ObjectBuilder::new(
            isa,
            mir_module.name.as_bytes().to_vec(),
            cranelift_module::default_libcall_names(),
        )
        .map_err(|e| crate::types::NuError::VMError {
            msg: format!("failed to create native object builder: {}", e),
            span: Span::default(),
        })?;
        let mut module = ObjectModule::new(object_builder);
        let mut builder_context = FunctionBuilderContext::new();

        let mut field_map = std::collections::HashMap::new();
        let mut next_field_id = 0u8;
        let mut constants = Vec::new();
        for func in &mir_module.functions {
            for block in &func.blocks {
                for stmt in &block.stmts {
                    super::collect_field_and_consts(
                        stmt,
                        &mut field_map,
                        &mut next_field_id,
                        &mut constants,
                        &mir_module.foreign_functions,
                    );
                }
            }
        }

        let mut func_ids = Vec::with_capacity(mir_module.functions.len());
        let mut unboxed_ids = vec![None; mir_module.functions.len()];

        for (idx, func) in mir_module.functions.iter().enumerate() {
            let mut sig = module.make_signature();
            for _ in &func.params {
                sig.params.push(AbiParam::new(types::I64));
            }
            for _ in &func.captures {
                sig.params.push(AbiParam::new(types::I64));
            }
            sig.returns.push(AbiParam::new(types::I64));

            let fid = module
                .declare_function(&format!("nulang_fn_{}", idx), Linkage::Local, &sig)
                .map_err(|e| crate::types::NuError::VMError {
                    msg: format!("failed to declare object function '{}': {}", func.name, e),
                    span: Span::default(),
                })?;
            func_ids.push(fid);

            if codegen::is_all_int(func) {
                let mut ub_sig = module.make_signature();
                for _ in &func.params {
                    ub_sig.params.push(AbiParam::new(types::I64));
                }
                for _ in &func.captures {
                    ub_sig.params.push(AbiParam::new(types::I64));
                }
                ub_sig.returns.push(AbiParam::new(types::I64));
                let ub_fid = module
                    .declare_function(
                        &format!("nulang_fn_{}_unboxed", idx),
                        Linkage::Local,
                        &ub_sig,
                    )
                    .map_err(|e| crate::types::NuError::VMError {
                        msg: format!(
                            "failed to declare unboxed object function '{}': {}",
                            func.name, e
                        ),
                        span: Span::default(),
                    })?;
                unboxed_ids[idx] = Some(ub_fid);
            }
        }

        let mut entry_idx = None;
        for (idx, func) in mir_module.functions.iter().enumerate() {
            if let Some(ub_fid) = unboxed_ids[idx] {
                let mut ctx = codegen::AotContext::new(&mut module, &mut builder_context);
                ctx.func_ids = func_ids.clone();
                ctx.func_ids[idx] = ub_fid;
                ctx.field_map = field_map.clone();
                ctx.constants = constants.clone();
                ctx.foreign_functions = mir_module.foreign_functions.clone();
                codegen::compile_mir_function_body(
                    &mut ctx,
                    func,
                    idx,
                    ub_fid,
                    codegen::CompileMode::Unboxed,
                )
                .map_err(|e| crate::types::NuError::VMError {
                    msg: format!(
                        "native object compilation of unboxed '{}' failed: {}",
                        func.name, e
                    ),
                    span: Span::default(),
                })?;

                let mut wrapper = codegen::AotContext::new(&mut module, &mut builder_context);
                codegen::compile_boxing_wrapper(
                    &mut wrapper,
                    func.params.len(),
                    func_ids[idx],
                    ub_fid,
                )
                .map_err(|e| crate::types::NuError::VMError {
                    msg: format!(
                        "native object boxing wrapper for '{}' failed: {}",
                        func.name, e
                    ),
                    span: Span::default(),
                })?;
            } else {
                let mut ctx = codegen::AotContext::new(&mut module, &mut builder_context);
                ctx.func_ids = func_ids.clone();
                ctx.field_map = field_map.clone();
                ctx.constants = constants.clone();
                ctx.foreign_functions = mir_module.foreign_functions.clone();
                codegen::compile_mir_function_body(
                    &mut ctx,
                    func,
                    idx,
                    func_ids[idx],
                    codegen::CompileMode::Boxed,
                )
                .map_err(|e| crate::types::NuError::VMError {
                    msg: format!("native object compilation of '{}' failed: {}", func.name, e),
                    span: Span::default(),
                })?;
            }

            if func.name == "__main" || func.name == "main" {
                if entry_idx.is_none() || func.name == "__main" {
                    entry_idx = Some(idx);
                }
            }
        }

        let entry_symbol = if let Some(idx) = entry_idx {
            let mut sig = module.make_signature();
            sig.returns.push(AbiParam::new(types::I64));
            let wrapper_fid = module
                .declare_function("nulang_entry", Linkage::Export, &sig)
                .map_err(|e| crate::types::NuError::VMError {
                    msg: format!("failed to declare native object entry: {}", e),
                    span: Span::default(),
                })?;
            let mut wrapper = codegen::AotContext::new(&mut module, &mut builder_context);
            codegen::compile_entry_wrapper(&mut wrapper, wrapper_fid, func_ids[idx]).map_err(
                |e| crate::types::NuError::VMError {
                    msg: format!("failed to compile native object entry: {}", e),
                    span: Span::default(),
                },
            )?;
            Some("nulang_entry".to_string())
        } else {
            None
        };

        let product = module.finish();
        let bytes = product.emit().map_err(|e| crate::types::NuError::VMError {
            msg: format!("failed to serialize native object: {}", e),
            span: Span::default(),
        })?;

        Ok(Self {
            bytes,
            target: target.to_string(),
            entry_symbol,
            constants,
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn entry_symbol(&self) -> Option<&str> {
        self.entry_symbol.as_deref()
    }

    pub fn constants(&self) -> &[crate::bytecode::Constant] {
        &self.constants
    }

    pub fn write_to(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        std::fs::write(path, &self.bytes)
    }
}
