#include <iostream>
#include <fstream>
#include <string>

using namespace std;

int main(int argc, char* argv[]) {
    if (argc < 2) return 3;
    ifstream fin(argv[1]);
    int secret = 0;
    fin >> secret;

    int queries = 0;
    while (true) {
        string op;
        if (!(cin >> op)) {
            cerr << "Unexpected EOF reading query command" << endl;
            return 1;
        }
        if (op == "?") {
            int val;
            if (!(cin >> val)) return 1;
            queries++;
            if (queries > 50) {
                cerr << "Query limit exceeded: " << queries << endl;
                return 1;
            }
            if (val < secret) {
                cout << "<" << endl;
            } else if (val > secret) {
                cout << ">" << endl;
            } else {
                cout << "=" << endl;
            }
        } else if (op == "!") {
            int ans;
            if (!(cin >> ans)) return 1;
            if (ans == secret) {
                cerr << "Correct secret guessed in " << queries << " queries" << endl;
                return 0;
            } else {
                cerr << "Wrong answer: expected " << secret << ", found " << ans << endl;
                return 1;
            }
        } else {
            cerr << "Invalid query operation: " << op << endl;
            return 1;
        }
    }
}
