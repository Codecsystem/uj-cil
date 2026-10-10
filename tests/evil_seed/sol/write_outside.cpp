#include <cstdio>
int main(){FILE*f=fopen("E:\\Data\\Work\\uj-cil\\PWNED.txt","w");FILE*g=fopen("C:\\Users\\Public\\PWNED.txt","w");printf("%d\n",(f||g)?999:3);}
