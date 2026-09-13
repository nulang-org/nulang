import sys

def make_body(depth):
    """Generate a nested if/else body using parameters."""
    params = ['comp_rec', 'src', 'eff_start', 'q5', 'len', 'nr', 'env', 'elen', 'no_left', 'p']
    def gen(level):
        if level > depth:
            return "(nr * 262144) + p"
        v = params[level - 1]
        return f"(if {v} == 1 then (nr * 262144) + p else if {v} == 2 then ({gen(level + 1)}) else (nr * 262144) + p)"
    return gen(1)

depth = int(sys.argv[1])
print(f"fn low17(x: Int) -> Int {{ x }}")
curried = ' => '.join(f'fn({p})' for p in ['comp_rec', 'src', 'eff_start', 'q5', 'len', 'nr', 'env', 'elen', 'no_left', 'p'])
print(f"let f = {curried} => {make_body(min(depth, 10))} in 123")
