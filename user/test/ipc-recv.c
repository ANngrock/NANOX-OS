/*
 * bin/ipc-recv — M2 criterion 3 (scenarios m2-ipc, m2-ipc-overgrant),
 * receiving side.  Blocks in NX_SYS_IPC_RECV, checks the message, the
 * sender and that the attached handle carries exactly NX_M2_IPC_GRANT,
 * uses it for a permitted operation (read-only mapping of the page the
 * sender filled) and checks that operations needing the dropped rights are
 * refused.  a0 = endpoint handle with the RECV right, a1 = task id of
 * ipc-send.  Exit code 0 if everything matches, 2 otherwise.
 */
#include <nanox/m2test.h>

#include "nanox_user.h"

static char buf[NX_IPC_MSG_MAX + 1];

int64_t umain(uint64_t ep, uint64_t sender, uint64_t a2, uint64_t a3)
{
    struct nx_ipc_info info = {0, 0, 0, 0};
    u_printf("waiting for a message on handle 0x%lx\n", ep);
    int64_t n = nx_ipc_recv(ep, buf, NX_IPC_MSG_MAX, &info);
    if (n < 0) {
        u_printf("recv failed: %s\n", u_err(n));
        return 10;
    }
    const char *msg = NX_M2_IPC_MSG;
    uint64_t mlen = u_strlen(msg);
    buf[n] = 0;
    int msg_ok = (uint64_t)n == mlen && info.len == mlen && u_memeq(buf, msg, mlen);
    int sender_ok = info.sender_task == sender;
    u_printf("received len=%ld from=#%u (expected #%lu) message %s: \"%s\"\n", n,
             info.sender_task, sender, msg_ok ? "intact" : "WRONG", buf);

    int rights_ok = info.handle != 0 && info.rights == NX_M2_IPC_GRANT;
    u_printf("handle=0x%x rights=0x%x expected=0x%x\n", info.handle, info.rights,
             NX_M2_IPC_GRANT);

    /* Permitted: READ + MAP allow a read-only mapping. */
    int64_t r = nx_vmo_map(info.handle, NX_M2_IPC_RECV_VA, NX_PROT_READ);
    int data_ok = 0;
    if (r == 0) {
        const volatile uint8_t *page = (const volatile uint8_t *)(uintptr_t)NX_M2_IPC_RECV_VA;
        data_ok = 1;
        for (uint32_t i = 0; i < 4096; i++)
            data_ok &= page[i] == nx_m2_ipc_byte(i);
    }
    u_printf("map read-only at 0x%lx: %s, page contents %s\n", NX_M2_IPC_RECV_VA, u_err(r),
             data_ok ? "match the sender's" : "DIFFER");

    /* Not permitted: WRITE and DUPLICATE were dropped by the sender. */
    int64_t rw = nx_vmo_map(info.handle, NX_M2_IPC_RECV_VA2, NX_PROT_READ | NX_PROT_WRITE);
    int64_t dup = nx_handle_dup(info.handle, NX_RIGHT_READ);
    u_printf("map read-write: %s (expected EACCESS), duplicate: %s (expected EACCESS)\n",
             u_err(rw), u_err(dup));

    int ok = msg_ok && sender_ok && rights_ok && r == 0 && data_ok && rw == -NX_EACCESS &&
             dup == -NX_EACCESS;
    u_printf("verdict %s\n", ok ? "ok" : "FAILED");
    return ok ? 0 : 2;
}
