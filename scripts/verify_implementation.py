#!/usr/bin/env python3
import os
import re
import sys
import subprocess

# Target: keep the crate warning-free. Increase only temporarily and document
# the reason if a batch of warnings cannot be immediately fixed.
WARNINGS_BASELINE = 0


def check_warnings_for(feature_args, label):
    """Run cargo check --tests with the given feature args and fail if
    compiler warnings/errors are produced. Errors (not just warnings) are
    fatal here too: a feature combination that doesn't compile is exactly
    how the ai-runtime/core coupling bug (fixed 2026-08-01 — crate::ai::*
    used unconditionally outside its #[cfg(feature = "ai-runtime")] gate)
    slipped past this script for as long as it did — check_warnings()
    originally checked only the default feature set, while CI's
    minimal-build job (--no-default-features) was the only thing that
    would have caught it.
    """
    print(f"Running cargo check --tests {label} to count compiler warnings...")
    res = subprocess.run(
        ["cargo", "check", "--tests", "--message-format=short"] + feature_args,
        capture_output=True,
        text=True,
    )
    if res.returncode != 0:
        print(f"Error: cargo check --tests {label} failed.")
        print("STDOUT:")
        print(res.stdout)
        print("STDERR:")
        print(res.stderr)
        return False

    # Count individual warning lines, excluding cargo's summary lines.
    # With --message-format=short, cargo prefixes each diagnostic with
    # path:line:col:, so warnings do NOT start the line — match the
    # "warning: " diagnostic marker as a substring instead, while still
    # excluding the "generated N warning(s)" summary lines.
    count = len(
        [
            line
            for line in res.stderr.splitlines()
            if "warning: " in line
            and "generated" not in line
            and "warnings" not in line
        ]
    )

    print(f"cargo check --tests {label} warning count: {count} (baseline: {WARNINGS_BASELINE}).")

    if count > WARNINGS_BASELINE:
        print(
            f"Error: cargo check --tests {label} produced {count} warning(s), exceeding the baseline of {WARNINGS_BASELINE}."
        )
        print("Fix the warnings or document the reason for raising the baseline.")
        print(f"cargo check --tests {label} output:")
        print(res.stderr)
        return False

    print(f"Success: cargo check --tests {label} warning count is within baseline.")
    return True


def check_warnings():
    """Run cargo check --tests across every feature combination CI exercises
    (default, --no-default-features, --all-features) and fail if any of them
    produce compiler warnings/errors. Checking only the default feature set
    is not sufficient — see check_warnings_for's docstring.
    """
    return (
        check_warnings_for([], "(default features)")
        and check_warnings_for(["--no-default-features"], "(--no-default-features)")
        and check_warnings_for(["--all-features"], "(--all-features)")
    )

def verify_files():
    # 1. (compiler.rs has been removed; MIR pipeline is now exclusive.)
    # 2. Check vm.rs for Frame caller and leaked SConcat
    if os.path.exists("src/vm.rs"):
        with open("src/vm.rs", "r", encoding="utf-8") as f:
            content = f.read()
            if "caller: Option<Box<Frame>>" in content or "caller: Option<Box<Self>>" in content:
                print("Error: src/vm.rs still heap-allocates call frames via Box.")
                return False
            if ".leak().as_mut_ptr()" in content:
                print("Error: src/vm.rs still contains raw string leaking via .leak().")
                return False
    else:
        print("Error: src/vm.rs does not exist.")
        return False

    # 3. Check crdt_reg.rs for vector allocation in insert_at/delete_at
    if os.path.exists("src/runtime/crdt_reg.rs"):
        with open("src/runtime/crdt_reg.rs", "r", encoding="utf-8") as f:
            content = f.read()
            # check if live: Vec is still used in insert_at
            if "live: Vec" in content or "live.collect()" in content:
                print("Error: src/runtime/crdt_reg.rs still allocates temporary live vector in insert_at/delete_at.")
                return False
    else:
        print("Error: src/runtime/crdt_reg.rs does not exist.")
        return False

    # 4. Check timer.rs for BinaryHeap rebuild
    if os.path.exists("src/runtime/timer.rs"):
        with open("src/runtime/timer.rs", "r", encoding="utf-8") as f:
            content = f.read()
            if "new_heap" in content and "timers.pop()" in content:
                print("Error: src/runtime/timer.rs still drains and rebuilds the BinaryHeap on every tick.")
                return False
    else:
        print("Error: src/runtime/timer.rs does not exist.")
        return False

    # 5. Check distributed.rs for check-then-unwrap
    # The anti-pattern is `contains_key(k)` guarding a `map.get(k).unwrap()`
    # within the same statement. `contains_key` inside retain-filter closures
    # is legitimate, so match the guard->lookup->unwrap shape rather than any
    # substring co-occurrence.
    if os.path.exists("src/runtime/distributed.rs"):
        with open("src/runtime/distributed.rs", "r", encoding="utf-8") as f:
            content = f.read()
            if re.search(
                r"contains_key\s*\([^)]*\)[\s\S]{0,200}?\.get\([\s\S]{0,200}?unwrap\s*\(",
                content,
            ):
                print("Error: src/runtime/distributed.rs still performs check-then-unwrap lookup in get().")
                return False
    else:
        print("Error: src/runtime/distributed.rs does not exist.")
        return False

    # 6. Check main.rs / compiler.rs / vm.rs for JIT integration.
    # Escape analysis was intentionally reverted in v0.12 (per AGENTS.md and
    # README.md); it must remain dead code and not be wired into the
    # compiler/runtime pipeline.
    integrated_jit = False
    escape_analysis_dead = True
    for filename in ["src/main.rs", "src/vm.rs"]:
        if os.path.exists(filename):
            with open(filename, "r", encoding="utf-8") as f:
                content = f.read()
                if "tiered_execute_step" in content or "jit_session" in content:
                    integrated_jit = True
                if "EscapeAnalyzer" in content or "escape_analysis" in content:
                    # Any import/use in the main pipeline means it is wired.
                    escape_analysis_dead = False

    if not integrated_jit:
        print("Error: JIT/tiered_execute_step is not integrated into compiler/runtime pipeline.")
        return False

    if not escape_analysis_dead:
        print("Error: EscapeAnalyzer is referenced in the compiler/runtime pipeline; it should remain dead code after v0.12 revert.")
        return False

    # 7. Verify scheduler profiling is wired through the Runtime.
    scheduler_wired = False
    if os.path.exists("src/runtime/mod.rs"):
        with open("src/runtime/mod.rs", "r", encoding="utf-8") as f:
            content = f.read()
            if "scheduler_stats" in content and "reset_scheduler_stats" in content:
                scheduler_wired = True
    if not scheduler_wired:
        print("Error: Scheduler profiling statistics are not exposed through Runtime.")
        return False

    # 8. Verify cycle detector intra-node restriction is wired.
    intra_node_wired = False
    if os.path.exists("src/runtime/mod.rs") and os.path.exists("src/runtime/orca_cycle.rs"):
        with open("src/runtime/mod.rs", "r", encoding="utf-8") as f:
            rt_content = f.read()
        with open("src/runtime/orca_cycle.rs", "r", encoding="utf-8") as f:
            cd_content = f.read()
        if "set_local_actors" in cd_content and "set_local_actors" in rt_content:
            intra_node_wired = True
    if not intra_node_wired:
        print("Error: Cycle detector intra-node restriction is not wired in Runtime.")
        return False

    print("Success: All files passed implementation checks!")
    return True


def run_tests():
    """Run cargo test --lib (default features) and fail on any test failure.

    AGENTS.md documents this script as running 'cargo test' — it did not;
    check_warnings() only ever ran cargo check --tests (compiles, never
    executes). This is also RELEASE_CHECKLIST.md's first pre-flight box
    ("cargo test --lib — all tests pass"), previously unautomated. Runs
    default features only — check_warnings() already exercises all three
    feature configs for compile-cleanliness; running the full suite three
    times over would be slow for marginal additional coverage.
    """
    print("Running cargo test --lib (default features)...")
    res = subprocess.run(
        ["cargo", "test", "--lib", "--quiet"],
        capture_output=True,
        text=True,
    )
    if res.returncode != 0:
        print("Error: cargo test --lib failed.")
        print("STDOUT:")
        print(res.stdout)
        print("STDERR:")
        print(res.stderr)
        return False

    result_line = next(
        (line for line in res.stdout.splitlines() if line.startswith("test result:")),
        "test result: (not found in output)",
    )
    print(f"Success: cargo test --lib passed. {result_line}")
    return True


if __name__ == "__main__":
    if verify_files() and check_warnings() and run_tests():
        sys.exit(0)
    else:
        sys.exit(1)
