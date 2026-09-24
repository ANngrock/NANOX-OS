/* CRC-32, see nanox/crc32.h.  Nibble table: small and fast enough for the
 * block sizes of the store (M4). */
#include <nanox/crc32.h>

static const uint32_t NIBBLE[16] = {
    0x00000000u, 0x1DB71064u, 0x3B6E20C8u, 0x26D930ACu, 0x76DC4190u, 0x6B6B51F4u,
    0x4DB26158u, 0x5005713Cu, 0xEDB88320u, 0xF00F9344u, 0xD6D6A3E8u, 0xCB61B38Cu,
    0x9B64C2B0u, 0x86D3D2D4u, 0xA00AE278u, 0xBDBDF21Cu,
};

uint32_t nx_crc32_update(uint32_t crc, const void *data, uint64_t len)
{
    const uint8_t *p = data;
    crc = ~crc;
    while (len--) {
        crc ^= *p++;
        crc = (crc >> 4) ^ NIBBLE[crc & 15u];
        crc = (crc >> 4) ^ NIBBLE[crc & 15u];
    }
    return ~crc;
}
