int f(int n){volatile char buf[4096]; buf[0]=n; return f(n+1)+buf[0];}
int main(){return f(0);}
