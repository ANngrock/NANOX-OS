/*
 * tlstool: host front end of the M5 TLS 1.3 client (lib/tls, lib/crypto)
 * over an ordinary TCP socket, for interoperability tests against
 * OpenSSL (Python's ssl module) in tests/host/test_tls.py.
 *
 *   tlstool HOST_IP PORT NAME ANCHOR.der NOW SUITES [MESSAGE]
 *
 * NAME is the server name (SNI and certificate check), NOW the Unix time
 * for certificate validity, SUITES 1 (AES-128-GCM), 2 (ChaCha20-Poly1305)
 * or 3 (both).  With MESSAGE the tool sends it after the handshake and
 * prints what comes back until the server closes.  Output, one line:
 *   "tls ok suite=<name> sig=<scheme> chain=<n> depth=<n> echo=<text>"
 *   "tls error <nxe name> class=<class> alert=<n> detail=<chain detail>"
 */
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#include <nanox/drbg.h>
#include <nanox/nerr.h>
#include <nanox/tls.h>
#include <nanox/x509.h>

static int sock_send(void *ctx, const uint8_t *buf, uint32_t len)
{
    int fd = *(int *)ctx;
    while (len) {
        ssize_t n = send(fd, buf, len, MSG_NOSIGNAL);
        if (n <= 0)
            return NXE_CONN_RESET;
        buf += n;
        len -= (uint32_t)n;
    }
    return NXE_OK;
}

static int sock_recv(void *ctx, uint8_t *buf, uint32_t cap, uint32_t timeout_ms)
{
    int fd = *(int *)ctx;
    struct pollfd p = {fd, POLLIN, 0};
    int r = poll(&p, 1, (int)timeout_ms);
    if (r == 0)
        return -NXE_NET_TIMEOUT;
    if (r < 0)
        return -NXE_NET_IO;
    ssize_t n = recv(fd, buf, cap, 0);
    if (n < 0)
        return errno == ECONNRESET ? -NXE_CONN_RESET : -NXE_NET_IO;
    return (int)n;
}

static struct nx_drbg drbg;

static int rnd(void *ctx, void *buf, uint32_t len)
{
    (void)ctx;
    return nx_drbg_generate(&drbg, buf, len, 0, 0) == 0 ? NXE_OK : NXE_LOC_ENTROPY;
}

static struct tls_conn conn;
static uint8_t anchor_der[8192];

int main(int argc, char **argv)
{
    if (argc < 7) {
        fprintf(stderr, "usage: tlstool HOST_IP PORT NAME ANCHOR.der NOW SUITES [MESSAGE]\n");
        return 2;
    }
    FILE *f = fopen(argv[4], "rb");
    if (!f) {
        perror(argv[4]);
        return 2;
    }
    size_t alen = fread(anchor_der, 1, sizeof(anchor_der), f);
    fclose(f);
    struct nx_cert anchor;
    if (nx_x509_parse(anchor_der, (uint32_t)alen, &anchor) != NXE_OK) {
        printf("tls error anchor_unparsable\n");
        return 1;
    }
    uint8_t seed[48];
    int ufd = open("/dev/urandom", O_RDONLY);
    if (ufd < 0 || read(ufd, seed, sizeof(seed)) != (ssize_t)sizeof(seed))
        return 2;
    close(ufd);
    nx_drbg_instantiate(&drbg, seed, sizeof(seed), (const uint8_t *)"tlstool", 7);

    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in sa;
    memset(&sa, 0, sizeof(sa));
    sa.sin_family = AF_INET;
    sa.sin_port = htons((uint16_t)atoi(argv[2]));
    inet_pton(AF_INET, argv[1], &sa.sin_addr);
    if (connect(fd, (struct sockaddr *)&sa, sizeof(sa)) != 0) {
        printf("tls error %s\n", nx_err_name(NXE_CONN_REFUSED));
        return 1;
    }
    struct tls_io io = {&fd, sock_send, sock_recv};
    struct tls_config cfg;
    memset(&cfg, 0, sizeof(cfg));
    cfg.host = argv[3];
    cfg.anchors = &anchor;
    cfg.nanchors = 1;
    cfg.now = atoll(argv[5]);
    cfg.suites = (uint32_t)atoi(argv[6]);
    cfg.random = rnd;
    cfg.timeout_ms = 5000;
    int r = tls_connect(&conn, &io, &cfg);
    if (r != NXE_OK) {
        printf("tls error %s class=%s alert=%d detail=%s\n", nx_err_name(r),
               nx_err_class_name(nx_err_class(r)), conn.alert == 0xFF ? -1 : conn.alert,
               conn.chain_detail[0] ? conn.chain_detail : "-");
        close(fd);
        return 1;
    }
    char echo[4096] = "-";
    if (argc > 7) {
        r = tls_write(&conn, argv[7], (uint32_t)strlen(argv[7]));
        uint32_t have = 0;
        while (r == NXE_OK && have < sizeof(echo) - 1) {
            int n = tls_read(&conn, echo + have, (uint32_t)(sizeof(echo) - 1 - have), 5000);
            if (n <= 0) {
                if (n < 0)
                    r = -n;
                break;
            }
            have += (uint32_t)n;
        }
        echo[have] = 0;
        if (r != NXE_OK) {
            printf("tls error %s class=%s during=data\n", nx_err_name(r),
                   nx_err_class_name(nx_err_class(r)));
            close(fd);
            return 1;
        }
    }
    printf("tls ok suite=%s sig=%s chain=%u depth=%u records_in=%lu echo=%s\n",
           tls_suite_name(conn.suite), tls_sig_name(conn.sig_scheme), conn.chain_len,
           conn.chain_depth, (unsigned long)conn.records_in, echo);
    tls_close(&conn);
    close(fd);
    return 0;
}
