/* Minimal host unit-test helpers (no external framework). */
#ifndef NANOX_TEST_H
#define NANOX_TEST_H

#include <stdio.h>

extern int nx_test_checks;
extern int nx_test_failures;

#define CHECK(cond)                                                                        \
    do {                                                                                   \
        nx_test_checks++;                                                                  \
        if (!(cond)) {                                                                     \
            nx_test_failures++;                                                            \
            fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond);       \
        }                                                                                  \
    } while (0)

#define CHECK_EQ_INT(a, b)                                                                 \
    do {                                                                                   \
        long long va_ = (long long)(a), vb_ = (long long)(b);                              \
        nx_test_checks++;                                                                  \
        if (va_ != vb_) {                                                                  \
            nx_test_failures++;                                                            \
            fprintf(stderr, "%s:%d: CHECK_EQ failed: %s == %lld, expected %s == %lld\n",   \
                    __FILE__, __LINE__, #a, va_, #b, vb_);                                 \
        }                                                                                  \
    } while (0)

void test_bootinfo(void);
void test_sha256(void);
void test_elf(const char *kernel_path);
void test_mmap(void);

#endif
