#pragma once
#include "capture-types.h"

// Private experiment ABI, independent of SoundSync's network protocol.
// METHOD_BUFFERED, custom device type 0x8337, FILE_READ_DATA. No pointer fields.
constexpr unsigned long SS_ATMOS_READ = (0x8337ul << 16) | (1ul << 14) | (0x800ul << 2);
constexpr uint32_t SS_ATMOS_VERSION = 1;
constexpr uint32_t SS_ATMOS_ACTIVE = 1, SS_ATMOS_PROTECTED = 2, SS_ATMOS_CONFLICT = 4;
constexpr uint32_t SS_ATMOS_MAX_READ = 65536;
struct CaptureHeader {
    uint32_t version, header_bytes;
    uint64_t generation, start_offset, produced_bytes, dropped_bytes;
    uint32_t data_bytes, flags;
    uint32_t sample_rate, channels, bits_per_sample, block_align;
    uint8_t subtype[16];
    uint64_t qpc_frequency, last_write_qpc;
};
static_assert(sizeof(CaptureHeader) == 96, "Capture ABI layout changed");
