#include "testlib.h"

int main(int argc, char* argv[]) {
    registerTestlibCmd(argc, argv);
    long long pans = ouf.readLong();
    long long jans = ans.readLong();
    if (pans == jans) {
        quitf(_ok, "correct sum: %lld", pans);
    } else {
        quitf(_wa, "expected %lld, found %lld", jans, pans);
    }
    return 0;
}
