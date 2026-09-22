#include <cstdint>
#include <iostream>

int main() {
    std::int64_t sum = 0;
    std::int64_t i = 0;
    while (i < 9000000) {
        sum = sum + i * 3 - i / 7;
        ++i;
    }
    std::cout << sum << '\n';
    return 0;
}
