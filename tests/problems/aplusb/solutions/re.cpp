int main() {
    volatile int* p = (volatile int*)0;
    *p = 42;
    return 0;
}
