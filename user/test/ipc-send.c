/*
 * bin/ipc-send — M2 criterion 3 (scenarios m2-ipc, m2-ipc-overgrant),
 * sending side.  Creates a one-page memory object, fills it through a
 * read-write mapping and sends NX_M2_IPC_MSG with the object's handle,
 * keeping only NX_M2_IPC_GRANT of its rights.  a0 = endpoint handle with
 * the SEND right.  Exit code 0 when every call returned what the rules in
 * docs/m2-kernel.md require.
 */
#include <nanox/m2test.h>

#include "nanox_user.h"

int64_t umain(uint64_t ep, uint64_t a1, uint64_t a2, uint64_t a3)
{
    int bad = 0;
    int64_t h = nx_vmo_create(1);
    if (h <= 0) {
        u_printf("vmo_create failed: %s\n", u_err(h));
        return 10;
    }
    int64_t r = nx_vmo_map((uint64_t)h, NX_M2_IPC_SEND_VA, NX_PROT_READ | NX_PROT_WRITE);
    if (r != 0) {
        u_printf("vmo_map failed: %s\n", u_err(r));
        return 11;
    }
    volatile uint8_t *page = (volatile uint8_t *)(uintptr_t)NX_M2_IPC_SEND_VA;
    for (uint32_t i = 0; i < 4096; i++)
        page[i] = nx_m2_ipc_byte(i);
    u_printf("vmo handle=0x%lx mapped rw at 0x%lx, page filled\n", (uint64_t)h,
             NX_M2_IPC_SEND_VA);

    const char *msg = NX_M2_IPC_MSG;
    uint64_t len = u_strlen(msg);
    /* A right the handle does not have cannot be granted; the failed call
     * leaves the handle where it is. */
    r = nx_ipc_send(ep, msg, len, (uint64_t)h, NX_M2_IPC_GRANT | NX_RIGHT_SEND);
    u_printf("send granting an extra right (0x%x): %s (expected EACCESS)\n",
             NX_M2_IPC_GRANT | NX_RIGHT_SEND, u_err(r));
    bad |= r != -NX_EACCESS;

    r = nx_ipc_send(ep, msg, len, (uint64_t)h, NX_M2_IPC_GRANT);
    u_printf("send len=%lu granting 0x%x: %s\n", len, NX_M2_IPC_GRANT, u_err(r));
    bad |= r != 0;

    /* The handle moved into the message: it is gone from this task. */
    r = nx_handle_close((uint64_t)h);
    u_printf("close of the sent handle: %s (expected EBADHANDLE)\n", u_err(r));
    bad |= r != -NX_EBADHANDLE;
    return bad ? 1 : 0;
}
