def add(x, y):
    return x + y

sum_value = 0
i = 0
while i < 9_000_000:
    sum_value = add(sum_value, i * 3 - i // 7)
    if sum_value > 1_000_000_000:
        sum_value -= 1_000_000_000
    i += 1
print(sum_value)
