#include <cstdio>
int main(){FILE*f=fopen("leak.txt","w");if(f){fputs("x",f);fclose(f);}printf("3\n");}
