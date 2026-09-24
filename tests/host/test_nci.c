/* Host tests for the NCI wire format of the executor (user/core/nci.c). */
#include <stdint.h>
#include <string.h>

#include "nci.h"
#include "test.h"

static int parse(const char *line, struct nci_req *r)
{
    return nci_parse(line, (uint32_t)strlen(line), r);
}

static void test_parse_ok(void)
{
    struct nci_req r;
    CHECK_EQ_INT(parse("REQ t1-2 task.spawn program=load", &r), NCI_OK);
    CHECK(strcmp(r.id, "t1-2") == 0);
    CHECK(strcmp(r.op, "task.spawn") == 0);
    CHECK_EQ_INT(r.nkv, 1);
    CHECK(strcmp(nci_get(&r, "program"), "load") == 0);
    CHECK(nci_get(&r, "target") == 0);

    CHECK_EQ_INT(parse("REQ a task.terminate  target=task/00000000000000ff/5   expect_rev=2\r", &r),
                 NCI_OK);
    CHECK_EQ_INT(r.nkv, 2);
    uint64_t v = 0;
    CHECK_EQ_INT(nci_get_u64(&r, "expect_rev", &v), 1);
    CHECK_EQ_INT(v, 2);
    CHECK_EQ_INT(nci_get_u64(&r, "missing", &v), 0);
    CHECK_EQ_INT(nci_get_u64(&r, "target", &v), -1); /* not a number */

    CHECK_EQ_INT(parse("REQ x system.describe", &r), NCI_OK);
    CHECK_EQ_INT(r.nkv, 0);
}

static void test_parse_errors(void)
{
    struct nci_req r;
    CHECK_EQ_INT(parse("", &r), NCI_E_SYNTAX);
    CHECK(strcmp(r.id, "-") == 0);
    CHECK_EQ_INT(parse("RES a task.list", &r), NCI_E_SYNTAX);
    CHECK_EQ_INT(parse("REQ", &r), NCI_E_SYNTAX);
    CHECK_EQ_INT(parse("REQ a", &r), NCI_E_SYNTAX);
    CHECK_EQ_INT(parse("REQ a$ task.list", &r), NCI_E_ID);
    CHECK_EQ_INT(parse("REQ 012345678901234567890123456789012 task.list", &r), NCI_E_ID);
    CHECK_EQ_INT(parse("REQ a Task.list", &r), NCI_E_OP);
    CHECK_EQ_INT(parse("REQ a .list", &r), NCI_E_OP);
    CHECK_EQ_INT(parse("REQ a task.list x", &r), NCI_E_KV);
    CHECK(strcmp(r.id, "a") == 0); /* the id is known for the rejection */
    CHECK_EQ_INT(parse("REQ a task.list =v", &r), NCI_E_KV);
    CHECK_EQ_INT(parse("REQ a task.list k=", &r), NCI_E_KV);
    CHECK_EQ_INT(parse("REQ a task.list K=v", &r), NCI_E_KV);
    CHECK_EQ_INT(parse("REQ a task.list k=v\"", &r), NCI_E_KV);
    CHECK_EQ_INT(parse("REQ a task.list k=1 k=2", &r), NCI_E_DUPKEY);
    CHECK_EQ_INT(parse("REQ a task.list a=1 b=1 c=1 d=1 e=1 f=1 g=1 h=1 i=1", &r),
                 NCI_E_TOO_MANY);
    char longv[NCI_VAL_MAX + 40];
    strcpy(longv, "REQ a task.list k=");
    size_t n = strlen(longv);
    memset(longv + n, 'x', NCI_VAL_MAX + 1);
    longv[n + NCI_VAL_MAX + 1] = 0;
    CHECK_EQ_INT(parse(longv, &r), NCI_E_KV);
    longv[n + NCI_VAL_MAX] = 0; /* exactly the maximum */
    CHECK_EQ_INT(parse(longv, &r), NCI_OK);
    char huge[NCI_LINE_MAX + 20];
    memset(huge, 'a', sizeof(huge));
    memcpy(huge, "REQ a task.list ", 16);
    huge[sizeof(huge) - 1] = 0;
    CHECK_EQ_INT(parse(huge, &r), NCI_E_TOO_LONG);
    CHECK(strcmp(nci_strerror(NCI_E_DUPKEY), "duplicate_argument") == 0);
}

static void test_fingerprint(void)
{
    struct nci_req a, b;
    parse("REQ x task.spawn program=load", &a);
    parse("REQ y task.spawn program=load", &b);
    CHECK(nci_fingerprint(&a) == nci_fingerprint(&b)); /* the id is not part of it */
    parse("REQ x task.spawn program=hello", &b);
    CHECK(nci_fingerprint(&a) != nci_fingerprint(&b));
    parse("REQ x task.spawn programl=oad", &b);
    CHECK(nci_fingerprint(&a) != nci_fingerprint(&b)); /* key/value boundary counts */
    parse("REQ x task.inspect program=load", &b);
    CHECK(nci_fingerprint(&a) != nci_fingerprint(&b));
}

static void test_ref(void)
{
    char buf[NCI_REF_MAX];
    nci_ref_format(buf, 0x0123456789abcdefull, 42);
    CHECK(strcmp(buf, "task/0123456789abcdef/42") == 0);
    nci_ref_format(buf, 0x1, 7);
    CHECK(strcmp(buf, "task/0000000000000001/7") == 0);
    uint64_t boot = 0;
    uint32_t id = 0;
    CHECK_EQ_INT(nci_ref_parse("task/0123456789abcdef/42", &boot, &id), 1);
    CHECK(boot == 0x0123456789abcdefull);
    CHECK_EQ_INT(id, 42);
    CHECK_EQ_INT(nci_ref_parse("task/0123456789abcdef/0", &boot, &id), 0);
    CHECK_EQ_INT(nci_ref_parse("task/0123456789ABCDEF/1", &boot, &id), 0);
    CHECK_EQ_INT(nci_ref_parse("task/0123456789abcde/1", &boot, &id), 0);
    CHECK_EQ_INT(nci_ref_parse("task/0123456789abcdef/", &boot, &id), 0);
    CHECK_EQ_INT(nci_ref_parse("task/0123456789abcdef/4294967296", &boot, &id), 0);
    CHECK_EQ_INT(nci_ref_parse("vmo/0123456789abcdef/1", &boot, &id), 0);
    CHECK_EQ_INT(nci_ref_parse("task/", &boot, &id), 0);
}

static void test_builder(void)
{
    char mem[32];
    struct nci_buf b;
    nb_init(&b, mem, sizeof(mem));
    nb_str(&b, "RES a OK");
    nb_kv_u64(&b, "n", 18446744073709551615ull);
    CHECK(strcmp(mem, "RES a OK n=18446744073709551615") == 0);
    CHECK(!b.overflow);
    nb_char(&b, 'x'); /* 31 bytes used + NUL: full */
    CHECK(b.overflow);
    CHECK_EQ_INT(strlen(mem), 31);
    nb_init(&b, mem, sizeof(mem));
    nb_kv_i64(&b, "e", -5);
    nb_hex(&b, 0xab, 4);
    nb_hex(&b, 0, 0);
    CHECK(strcmp(mem, " e=-500ab0") == 0);
    CHECK(nci_value_ok("task/00/1"));
    CHECK(!nci_value_ok(""));
    CHECK(!nci_value_ok("a b"));
}

void test_nci(void)
{
    test_parse_ok();
    test_parse_errors();
    test_fingerprint();
    test_ref();
    test_builder();
}
