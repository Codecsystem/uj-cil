#include <iostream>
#include <string>

using namespace std;

int main() {
    int l = 1, r = 100;
    while (l <= r) {
        int mid = l + (r - l) / 2;
        cout << "? " << mid << endl;
        string resp;
        if (!(cin >> resp)) break;
        if (resp == "=") {
            cout << "! " << mid << endl;
            return 0;
        } else if (resp == "<") {
            l = mid + 1;
        } else if (resp == ">") {
            r = mid - 1;
        }
    }
    return 0;
}
