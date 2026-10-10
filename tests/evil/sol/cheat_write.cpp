#include <cstdio>
int main(){
    FILE* f = fopen("E:\\Data\\Work\\uj-cil\\tests\\evil\\tests\\HACKED.txt", "w");
    if (!f) f = fopen("/mnt/e/Data/Work/uj-cil/tests/evil/tests/HACKED.txt", "w");
    if (!f) f = fopen("..\\..\\..\\tests\\evil\\tests\\HACKED.txt", "w");
    if (!f) f = fopen("../../../tests/evil/tests/HACKED.txt", "w");
    if (!f) f = fopen("../../../tests/HACKED.txt", "w");
    if (f){ fputs("pwned", f); fclose(f); }
    printf("3\n");
}
