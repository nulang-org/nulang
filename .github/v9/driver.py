#!/usr/bin/env python3
from pathlib import Path

source_path = Path(".github/v9/apply.py")
source = source_path.read_text()

old = (
    "for old_count in [50, 30, 20, 25]:\n"
    "    replace_once(\n"
    "        \"src/runtime/cluster_dst.rs\",\n"
    "        f'''        let seeds = crate::dst::dst_seed_count({old_count});\n\n"
    "        for seed in 0..seeds {{\n"
    "''',\n"
    "        f'''        for seed in crate::dst::dst_seeds({old_count}) {{\n"
    "''',\n"
    "    )\n"
)

new = (
    "replace_once(\n"
    "    \"src/runtime/cluster_dst.rs\",\n"
    "    \"\"\"        let seeds = crate::dst::dst_seed_count(50);\\n"
    "        const ROUNDS: u64 = 40;\\n\\n"
    "        for seed in 0..seeds {\\n\"\"\",\n"
    "    \"\"\"        const ROUNDS: u64 = 40;\\n\\n"
    "        for seed in crate::dst::dst_seeds(50) {\\n\"\"\",\n"
    ")\n"
    "for old_count in [30, 20, 25]:\n"
    "    replace_once(\n"
    "        \"src/runtime/cluster_dst.rs\",\n"
    "        f\"\"\"        let seeds = crate::dst::dst_seed_count({old_count});\\n\\n"
    "        for seed in 0..seeds {{\\n\"\"\",\n"
    "        f\"\"\"        for seed in crate::dst::dst_seeds({old_count}) {{\\n\"\"\",\n"
    "    )\n"
)

count = source.count(old)
if count != 1:
    raise SystemExit(f"apply.py: expected one cluster DST migration block, found {count}")
source = source.replace(old, new, 1)
compile(source, str(source_path), "exec")
exec(source, {"__name__": "__main__"})
