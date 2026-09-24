#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include <nanox/sha256.h>

#include "test.h"

static void hex(const uint8_t *d, char *out)
{
    static const char h[] = "0123456789abcdef";
    for (int i = 0; i < 32; i++) {
        out[2 * i] = h[d[i] >> 4];
        out[2 * i + 1] = h[d[i] & 15];
    }
    out[64] = 0;
}

static void check_vector(const void *msg, size_t len, const char *expect)
{
    uint8_t d[32];
    char s[65];
    nx_sha256(msg, len, d);
    hex(d, s);
    CHECK(strcmp(s, expect) == 0);
    if (strcmp(s, expect) != 0)
        fprintf(stderr, "  got %s\n  exp %s\n", s, expect);
}

void test_sha256(void)
{
    /* FIPS 180-4 / NIST CAVP example vectors. */
    check_vector("", 0, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    check_vector("abc", 3, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    const char *m448 = "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    check_vector(m448, strlen(m448),
                 "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
    const char *m896 =
        "abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklm"
        "nopqklmnopqrlmnopqrsmnopqrstnopqrstu";
    check_vector(m896, strlen(m896),
                 "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1");
    char *million = malloc(1000000);
    memset(million, 'a', 1000000);
    check_vector(million, 1000000,
                 "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");

    /* Streaming with irregular chunk sizes gives the one-shot digest. */
    uint8_t one[32], inc[32];
    nx_sha256(million, 1000000, one);
    struct nx_sha256 ctx;
    nx_sha256_init(&ctx);
    size_t off = 0, step = 1;
    while (off < 1000000) {
        size_t n = step < 1000000 - off ? step : 1000000 - off;
        nx_sha256_update(&ctx, million + off, n);
        off += n;
        step = step * 7 % 131 + 1;
    }
    nx_sha256_final(&ctx, inc);
    CHECK(memcmp(one, inc, 32) == 0);
    free(million);

    /* Padding boundaries: 55, 56, 63, 64, 65 bytes all differ and are stable. */
    uint8_t buf[65], prev[32] = {0}, d[32];
    memset(buf, 0x5A, sizeof(buf));
    size_t lens[] = {55, 56, 63, 64, 65};
    for (size_t i = 0; i < 5; i++) {
        nx_sha256(buf, lens[i], d);
        CHECK(memcmp(d, prev, 32) != 0);
        memcpy(prev, d, 32);
    }
}
