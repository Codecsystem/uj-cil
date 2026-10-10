#include <iostream>
#include <fstream>
#include <cstdlib>
#include <cmath>

using namespace std;

int main(int argc, char* argv[]) {
    if (argc < 7) {
        return 1;
    }
    // spj <in> <out> <ans> <score> <score_file> <msg_file>
    ifstream fin(argv[1]);
    ifstream fout(argv[2]);
    ifstream fans(argv[3]);
    double full_score = atof(argv[4]);
    ofstream fscore(argv[5]);
    ofstream fmsg(argv[6]);

    long long pans = 0, jans = 0;
    if (!(fout >> pans)) {
        fscore << "0\n";
        fmsg << "Empty output\n";
        return 0;
    }
    fans >> jans;

    if (pans == jans) {
        fscore << full_score << "\n";
        fmsg << "Perfect match\n";
    } else if (std::abs(pans - jans) == 1) {
        fscore << (full_score * 0.4) << "\n";
        fmsg << "Close answer (diff 1), 40% score\n";
    } else {
        fscore << "0\n";
        fmsg << "Wrong answer\n";
    }

    return 0;
}
