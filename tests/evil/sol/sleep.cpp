#if defined(_WIN32)
#include <windows.h>
int main(){Sleep(100000);}
#else
#include <unistd.h>
int main(){sleep(100);}
#endif
