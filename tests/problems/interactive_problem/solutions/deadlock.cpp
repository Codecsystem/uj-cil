#include <iostream>
#include <string>

using namespace std;

int main() {
    // Deliberately do not output anything and wait on stdin to trigger deadlock / idleness limit
    string dummy;
    cin >> dummy;
    return 0;
}
