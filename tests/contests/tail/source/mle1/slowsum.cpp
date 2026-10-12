#include <iostream>
#include <vector>
using namespace std;
int main() {
    int n;
    if (cin >> n) {
        if (n == 20) {
            // Allocate 500MB (limit is 256MB)
            vector<char> v(500ULL * 1024 * 1024, 1);
            cout << v[0] << "\n";
            return 0;
        }
        long long sum = 0;
        for (int i = 0; i < n; ++i) {
            long long x;
            cin >> x;
            sum += x;
        }
        cout << sum << "\n";
    }
    return 0;
}
