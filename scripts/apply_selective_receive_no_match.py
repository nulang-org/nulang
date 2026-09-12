#!/usr/bin/env python3
"""Finish #143 by preventing selective-receive no-match fallback consumption.

Uses exact source matches and fails closed on drift. This helper is temporary
and is removed before the review PR is finalized.
"""

from pathlib import Path

# MIR lowering: a selective receive that exhausted candidates must not fall
# through to legacy pop-any Receive, because that can consume a guard-rejected
# or unrelated message. Plain arm-less receive still takes the early Receive
# path above this block.
mir_lower = Path("src/mir_lower.rs")
text = mir_lower.read_text()
old = '''        match after {
            Some((_, timeout_body)) => {
                self.lower_body_into(timeout_body, dst, join)?;
            }
            None => {
                self.b.assign(dst, mir::RValue::Receive);
                self.b.terminate(mir::Terminator::Jump(join));
            }
        }
'''
new = '''        match after {
            Some((_, timeout_body)) => {
                self.lower_body_into(timeout_body, dst, join)?;
            }
            None => {
                // Selective receive is non-consuming on no match. In
                // particular, a candidate rejected by a pattern/guard must
                // remain queued for a future receive rather than falling
                // through to the legacy pop-any Receive operation.
                self.b
                    .assign(dst, mir::RValue::Const(Constant::Nil));
                self.b.terminate(mir::Terminator::Jump(join));
            }
        }
'''
if text.count(old) != 1:
    raise SystemExit(f"mir_lower no-match pattern count: {text.count(old)}")
text = text.replace(old, new, 1)
old_doc = '''    /// When nothing matches (scan returns None → arm-count sentinel), the
    /// no-match block runs the legacy pop-any `Receive` or, with `after`,
    /// the timeout body.
'''
new_doc = '''    /// When nothing matches (scan returns None → arm-count sentinel), an
    /// untimed selective receive yields nil without consuming any queued
    /// message; with `after`, the timeout body runs. Plain arm-less `receive`
    /// still uses the legacy pop-any `Receive` path above.
'''
if text.count(old_doc) != 1:
    raise SystemExit(f"mir_lower doc pattern count: {text.count(old_doc)}")
mir_lower.write_text(text.replace(old_doc, new_doc, 1))

# MIR contract docs: Receive is now only the explicit/arm-less pop-any op,
# never the selective no-match fallback.
mir = Path("src/mir.rs")
text = mir.read_text()
old = '''    /// `receive { | Behavior(params) => expr ... }` with no arms (or in the
    /// no-match fallback block): pop the next message from the actor's
    /// mailbox; evaluates to its first payload value (nil when the mailbox
    /// is empty or outside an actor context).
'''
new = '''    /// Legacy pop-any receive used by an arm-less `receive`: pop the next
    /// message from the actor's mailbox; evaluates to its first payload value
    /// (nil when the mailbox is empty or outside an actor context). Selective
    /// receive no-match paths must not use this operation because rejected or
    /// unrelated messages must remain queued.
'''
if text.count(old) != 1:
    raise SystemExit(f"mir Receive doc pattern count: {text.count(old)}")
mir.write_text(text.replace(old, new, 1))

# Update the existing codegen regression: the untimed selective form still
# uses ReceiveMatch, but must not emit legacy Receive as a fallback.
codegen = Path("src/mir_codegen.rs")
text = codegen.read_text()
old = '''        assert!(
            module
                .instructions
                .iter()
                .any(|i| i.opcode == OpCode::Receive),
            "plain receive must keep the legacy fallback"
        );
'''
new = '''        assert!(
            !module
                .instructions
                .iter()
                .any(|i| i.opcode == OpCode::Receive),
            "selective receive must not consume a rejected/nonmatching message via legacy Receive"
        );
'''
if text.count(old) != 1:
    raise SystemExit(f"mir_codegen receive fallback assertion count: {text.count(old)}")
codegen.write_text(text.replace(old, new, 1))
