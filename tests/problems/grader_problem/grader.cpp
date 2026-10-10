#include <iostream>
#include "aplusb.h"

int main() {
    int a, b;
    if (std::cin >> a >> b) {
        std::cout << add(a, b) << std::endl;
    }
    return 0;
}
