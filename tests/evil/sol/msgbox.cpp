#if defined(_WIN32)
#include <windows.h>
int main(){MessageBoxA(NULL,"hi","hi",0);return 0;}
#else
int main(){return 0;}
#endif
