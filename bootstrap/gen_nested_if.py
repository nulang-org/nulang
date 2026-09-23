import sys

def make_body(depth):
    """Generate a nested if/else body with given depth."""
    def gen(level):
        if level > depth:
            return "(nr * 262144) + p"
        v = f"v{level}"
        return f"(if {v} == 1 then (nr * 262144) + p else if {v} == 2 then ({gen(level + 1)}) else (nr * 262144) + p)"
    return gen(1)

depth = int(sys.argv[1])
print(f"fn low17(x: Int) -> Int {{ x }}")
print(f"let f = fn(a) => fn(b) => fn(c) => fn(d) => fn(e) => fn(h) => fn(i) => fn(j) => fn(k) => {make_body(depth)} in 123")
