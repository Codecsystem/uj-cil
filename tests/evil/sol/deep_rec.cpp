#include <cstdio>
int f(int n){volatile char buf[64]; buf[0]=n; return n==0?0:f(n-1)+buf[0]%2;}
int main(){printf("%d\n", f(1000000)>=0?3:0);}
