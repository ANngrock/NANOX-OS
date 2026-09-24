#include <stdio.h>

#include "test.h"

int nx_test_checks;
int nx_test_failures;

int main(int argc, char **argv)
{
    test_sha256();
    test_elf(argc > 1 ? argv[1] : NULL);
    test_mmap();
    test_bootinfo();
    test_initramfs(argc > 2 ? argv[2] : NULL);
    printf("host tests: %d checks, %d failed\n", nx_test_checks, nx_test_failures);
    return nx_test_failures ? 1 : 0;
}
