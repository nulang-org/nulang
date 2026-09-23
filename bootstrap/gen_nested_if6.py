import sys

def make_body(depth):
    """Generate a nested if/else body with curried calls."""
    params = ['comp_rec', 'src', 'eff_start', 'q5', 'len', 'nr', 'env', 'elen', 'no_left', 'p']
    def gen(level):
        if level > depth:
            return "(nr * 262144) + p"
        v = params[level - 1]
        inner = gen(level + 1)
        return f"(if {v} == 1 then (nr * 262144) + p else if {v} == 2 then (let p{level} = skip_ws(src)({v} + 1)(len) in let inner{level} = comp_rec(src)(p{level})(len)(no_left)(0)(nr + {level})(env)(elen) in let r{level} = inner{level} >> 18 in let e{level} = low17(inner{level}) in {inner}) else (nr * 262144) + p)"
    return gen(1)

depth = int(sys.argv[1])
print(f"fn low17(x: Int) -> Int {{ x }}")
print(f"fn skip_ws(src: Int) -> fn(Int) -> fn(Int) -> Int {{ fn(pos) => fn(len) => (if pos >= len then pos else pos + 1) }}")
curried = ' => '.join(f'fn({p})' for p in ['comp_rec', 'src', 'eff_start', 'q5', 'len', 'nr', 'env', 'elen', 'no_left', 'p'])
print(f"let f = {curried} => {make_body(min(depth, 10))} in 123")
