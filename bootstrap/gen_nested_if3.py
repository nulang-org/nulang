import sys

def make_body(depth, params):
    def gen(level):
        if level > depth:
            return "(nr * 262144) + p"
        v = params[level - 1]
        return f"(if {v} == 1 then (nr * 262144) + p else if {v} == 2 then ({gen(level + 1)}) else (nr * 262144) + p)"
    return gen(1)

num_params = int(sys.argv[1])
depth = int(sys.argv[2])
params = ['a', 'b', 'c', 'd', 'e', 'h', 'i', 'j', 'k'][:num_params]
print(f"fn low17(x: Int) -> Int {{ x }}")
curried = ' => '.join(f'fn({p})' for p in params)
print(f"let f = {curried} => {make_body(min(depth, num_params), params)} in 123")
