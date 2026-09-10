#!/usr/bin/env python3
from pathlib import Path

source_path = Path('.github/v9/apply.py')
source = source_path.read_text()
old = '''for old_count in [50, 30, 20, 25]:
    replace_once(
        "src/runtime/cluster_dst.rs",
        f'''        let seeds = crate::dst::dst_seed_count({old_count});

        for seed in 0..seeds {{
''',
        f'''        for seed in crate::dst::dst_seeds({old_count}) {{
''',
    )
'''
new = '''replace_once(
    "src/runtime/cluster_dst.rs",
    """        let seeds = crate::dst::dst_seed_count(50);\n        const ROUNDS: u64 = 40;\n\n        for seed in 0..seeds {\n""",
    """        const ROUNDS: u64 = 40;\n\n        for seed in crate::dst::dst_seeds(50) {\n""",
)
for old_count in [30, 20, 25]:
    replace_once(
        "src/runtime/cluster_dst.rs",
        f"""        let seeds = crate::dst::dst_seed_count({old_count});\n\n        for seed in 0..seeds {{\n""",
        f"""        for seed in crate::dst::dst_seeds({old_count}) {{\n""",
    )
'''
count = source.count(old)
if count != 1:
    raise SystemExit(f'apply.py: expected one cluster DST migration block, found {count}')
source = source.replace(old, new, 1)
exec(compile(source, str(source_path), 'exec'), {'__name__': '__main__'})
