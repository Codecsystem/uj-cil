#include <vector>
#include <cstdio>
int main(){std::vector<char> v; v.resize(600u<<20, 1); printf("%d\n",(int)v[123]+2);}
