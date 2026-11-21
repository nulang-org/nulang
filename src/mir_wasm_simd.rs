use crate::ast::BinOp;
use crate::compute_ir::{ScalarType, VectorType, VectorWidth};
use crate::mir::{self, BlockId, LocalId, RValue, Stmt, Terminator};
use crate::type_metadata::KnownType;

/// WASM SIMD lane description backed by the backend-neutral compute IR.
///
/// The current WASM lowering emits 128-bit vectors, so both supported 64-bit
/// scalar types use two lanes. Keeping the wrapper here preserves the tiny
/// `is_float()` interface consumed by `mir_wasm.rs` while removing its former
/// dependency on the JIT-specific `SimdElemType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WasmVectorType {
    vector_type: VectorType,
}

impl WasmVectorType {
    fn from_known_type(known: KnownType) -> Self {
        let element = match known {
            KnownType::Float => ScalarType::F64,
            _ => ScalarType::I64,
        };
        let width = VectorWidth::fixed(2).expect("WASM SIMD uses a non-zero fixed lane width");
        Self {
            vector_type: VectorType::new(element, width),
        }
    }

    pub fn is_float(self) -> bool {
        matches!(self.vector_type.element, ScalarType::F32 | ScalarType::F64)
    }

    pub fn vector_type(self) -> VectorType {
        self.vector_type
    }
}

#[derive(Debug, Clone)]
pub struct VecLoop {
    pub header: BlockId,
    pub body: BlockId,
    pub exit: BlockId,
    pub induction: LocalId,
    pub array_a: LocalId,
    pub array_b: LocalId,
    pub array_c: LocalId,
    pub op: BinOp,
    pub lane_type: WasmVectorType,
}

pub fn find_vectorizable_loops(func: &mir::Function) -> Vec<VecLoop> {
    let mut loops = Vec::new();

    // We are looking for:
    // Header block:
    //   Branch { cond, then_(body), else_(exit) }
    //   where cond is from an Assign { cond, RValue::Binary(Lt, induction, ArrayLen(arr)) }
    // Wait, let's just find headers that branch to a body.
    for (header_id_usize, header_block) in func.blocks.iter().enumerate() {
        let header_id = BlockId(header_id_usize as u32);

        let Terminator::Branch {
            cond: _,
            then_,
            else_,
        } = header_block.terminator
        else {
            continue;
        };
        let body = then_;
        let exit = else_;

        let body_block = &func.blocks[body.0 as usize];

        // Ensure body ends with Jump(header)
        if body_block.terminator != Terminator::Jump(header_id) {
            continue;
        }

        // We look for statements in body block:
        // Assign { ai, ArrayLoad { a, i } }
        // Assign { bi, ArrayLoad { b, i } }
        // Assign { ci, Binary(op, ai, bi) }
        // ArrayStore { c, i, ci }
        // Assign { i, Binary(Add, i, const 1) }
        // Note: order might be slightly different. We need to match the data flow.

        let mut loads = std::collections::HashMap::new();
        let mut stores = Vec::new();
        let mut binaries = Vec::new();

        let mut has_other_control_flow = false;

        for stmt in &body_block.stmts {
            match stmt {
                Stmt::Assign {
                    dst,
                    op: RValue::ArrayLoad { arr, idx },
                } => {
                    loads.insert(*dst, (*arr, *idx));
                }
                Stmt::Assign {
                    dst,
                    op: RValue::Binary(op, left, right),
                } => {
                    binaries.push((*dst, *op, *left, *right));
                }
                Stmt::ArrayStore { arr, idx, src } => {
                    stores.push((*arr, *idx, *src));
                }
                Stmt::EnterHandle { .. }
                | Stmt::PopHandler
                | Stmt::Emit { .. }
                | Stmt::StateSet { .. }
                | Stmt::StoreFieldNamed { .. } => {
                    has_other_control_flow = true;
                }
                Stmt::Assign {
                    op: RValue::Call { .. },
                    ..
                } => {
                    has_other_control_flow = true;
                }
                _ => {}
            }
        }

        if has_other_control_flow || stores.is_empty() || loads.is_empty() {
            continue;
        }

        // Find the store: ArrayStore { c, i, ci }
        // Only one store supported for now
        if stores.len() != 1 {
            continue;
        }
        let (array_c, idx_c, src_c) = stores[0];

        // The induction variable is idx_c.
        let induction = idx_c;

        // Check if there is an increment for induction: i = i + 1
        let mut has_increment = false;
        for (dst, op, left, right) in &binaries {
            if *dst == induction && *op == BinOp::Add {
                if *left == induction || *right == induction {
                    has_increment = true;
                    break;
                }
            }
        }

        if !has_increment {
            continue;
        }

        // Find the binary op that produces src_c
        let mut found_binop = None;
        for (dst, op, left, right) in &binaries {
            if *dst == src_c {
                found_binop = Some((*op, *left, *right));
                break;
            }
        }

        let Some((op, left, right)) = found_binop else {
            continue;
        };

        // Left and right must come from array loads using the same induction variable
        let Some(&(array_a, idx_a)) = loads.get(&left) else {
            continue;
        };
        let Some(&(array_b, idx_b)) = loads.get(&right) else {
            continue;
        };

        if idx_a != induction || idx_b != induction {
            continue;
        }

        // Distinct destination array (no loop-carried dependency)
        if array_c == array_a || array_c == array_b {
            continue;
        }

        // Canonicalize element/vector shape through compute IR. The WASM
        // lowering remains 128-bit today, hence two 64-bit lanes.
        let lane_type = WasmVectorType::from_known_type(func.type_metadata.get_type(left.0 as usize));

        loops.push(VecLoop {
            header: header_id,
            body,
            exit,
            induction,
            array_a,
            array_b,
            array_c,
            op,
            lane_type,
        });
    }

    loops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_lane_type_is_compute_ir_f64x2() {
        let lane = WasmVectorType::from_known_type(KnownType::Float);
        let vector = lane.vector_type();

        assert!(lane.is_float());
        assert_eq!(vector.element, ScalarType::F64);
        assert_eq!(vector.width, VectorWidth::fixed(2).unwrap());
    }

    #[test]
    fn non_float_lane_type_is_compute_ir_i64x2() {
        let lane = WasmVectorType::from_known_type(KnownType::Int);
        let vector = lane.vector_type();

        assert!(!lane.is_float());
        assert_eq!(vector.element, ScalarType::I64);
        assert_eq!(vector.width, VectorWidth::fixed(2).unwrap());
    }
}
