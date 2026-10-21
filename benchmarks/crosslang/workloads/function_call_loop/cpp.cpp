#include <cstdint>
#include <iostream>

static std::int64_t add(std::int64_t x, std::int64_t y) {
    return x + y;
}

int main() {
    std::int64_t sum = 0;
    std::int64_t i = 0;
    while (i < 9000000) {
        sum = add(sum, i * 3 - i / 7);
        if (sum > 1000000000) {
            sum -= 1000000000;
        }
        ++i;
    }
    std::cout << sum << '\n';
    return 0;
}
