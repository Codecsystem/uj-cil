#include <thread>
#include <vector>
#include <cstdio>
int main(){std::vector<std::thread> t;for(int i=0;i<8;i++)t.emplace_back([]{volatile long long x=0;for(;;)x++;});for(auto&x:t)x.join();}
