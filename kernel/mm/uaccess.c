#include <nanox/syscall.h>

#include "uaccess.h"

int nx_user_range_ok(uint64_t va, uint64_t len)
{
    return va >= NX_USER_BASE && va < NX_USER_TOP && len <= NX_USER_TOP - va;
}

/* Physical address for user byte va, or 0 when not accessible as required. */
static uint64_t user_page(const struct nx_pt_env *env, uint64_t root, uint64_t va, int write)
{
    uint64_t pa, flags, size;
    if (nx_pt_query(env, root, va, &pa, &flags, &size) != NX_PT_OK)
        return 0;
    if (!(flags & NX_PTE_P) || !(flags & NX_PTE_U) || (write && !(flags & NX_PTE_W)))
        return 0;
    return pa;
}

int nx_uaccess_check(const struct nx_pt_env *env, uint64_t root, uint64_t va, uint64_t len,
                     int write)
{
    if (len == 0)
        return NX_OK;
    if (!nx_user_range_ok(va, len))
        return NX_EFAULT;
    uint64_t page = va & ~(NX_PAGE_4K - 1), end = va + len;
    for (; page < end; page += NX_PAGE_4K)
        if (!user_page(env, root, page, write))
            return NX_EFAULT;
    return NX_OK;
}

static int copy(const struct nx_pt_env *env, uint64_t root, uint64_t va, uint8_t *kbuf,
                uint64_t len, int to_user)
{
    int st = nx_uaccess_check(env, root, va, len, to_user);
    if (st != NX_OK)
        return st;
    while (len) {
        uint64_t pa = user_page(env, root, va, to_user);
        uint64_t off = va & (NX_PAGE_4K - 1), chunk = NX_PAGE_4K - off;
        if (chunk > len)
            chunk = len;
        uint8_t *p = (uint8_t *)env->virt(env->ctx, pa - off) + off;
        for (uint64_t i = 0; i < chunk; i++) {
            if (to_user)
                p[i] = kbuf[i];
            else
                kbuf[i] = p[i];
        }
        va += chunk;
        kbuf += chunk;
        len -= chunk;
    }
    return NX_OK;
}

int nx_uaccess_copy_in(const struct nx_pt_env *env, uint64_t root, void *dst, uint64_t va,
                       uint64_t len)
{
    return copy(env, root, va, dst, len, 0);
}

int nx_uaccess_copy_out(const struct nx_pt_env *env, uint64_t root, uint64_t va,
                        const void *src, uint64_t len)
{
    return copy(env, root, va, (uint8_t *)(uintptr_t)src, len, 1);
}
