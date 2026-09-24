/* CRC-32 (IEEE 802.3, reflected polynomial 0xEDB88320, initial value and
 * final XOR 0xFFFFFFFF) -- the same function as zlib.crc32, so host tools
 * written in Python can check it independently.  Freestanding; also
 * compiled for host tests.  Detects torn and damaged blocks; it is not a
 * cryptographic integrity check. */
#ifndef NANOX_LIB_CRC32_H
#define NANOX_LIB_CRC32_H

#include <stdint.h>

/* crc = nx_crc32_update(0, data, len) for a whole buffer; chaining calls
 * with the previous result continues the same checksum. */
uint32_t nx_crc32_update(uint32_t crc, const void *data, uint64_t len);

static inline uint32_t nx_crc32(const void *data, uint64_t len)
{
    return nx_crc32_update(0, data, len);
}

#endif
