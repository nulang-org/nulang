sum_value = 0
i = 0
while i < 9_000_000:
    sum_value = sum_value + i * 3 - i // 7
    i += 1
print(sum_value)
