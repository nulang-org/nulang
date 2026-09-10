#!/usr/bin/env python3
from pathlib import Path

p = Path("src/aot/codegen.rs")
text = p.read_text()
old = '''                    op,
                    mir::RValue::Binary(
                        crate::ast::BinOp::Div | crate::ast::BinOp::Mod | crate::ast::BinOp::Pow,
                        ..
                    ) | mir::RValue::ArrayLit(_)
'''
new = '''                    op,
                    mir::RValue::Binary(
                        crate::ast::BinOp::Div | crate::ast::BinOp::Mod | crate::ast::BinOp::Pow,
                        ..
                    ) | mir::RValue::Unary(crate::ast::UnOp::Neg, ..)
                        | mir::RValue::ArrayLit(_)
'''
if text.count(old) != 1:
    raise SystemExit(f"src/aot/codegen.rs: expected one is_all_int nil/object matcher, found {text.count(old)}")
p.write_text(text.replace(old, new, 1))
print("AOT unboxed eligibility tightened for unary Neg")
