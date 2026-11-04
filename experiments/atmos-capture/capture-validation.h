#pragma once
#include <string.h>
#include "capture-protocol.h"

struct CaptureValidation {
    bool started;
    uint64_t generation, next_offset, total_bytes;
    uint8_t subtype[16];

    // nullptr means accepted; empty polling responses do not start a recording.
    const char* Accept(const CaptureHeader& h, size_t returned) {
        if (h.version != SS_ATMOS_VERSION || h.header_bytes != sizeof(h)) return "Unsupported driver ABI";
        if (returned < sizeof(h) || h.data_bytes > SS_ATMOS_MAX_READ || h.data_bytes != returned - sizeof(h)) return "Invalid response length";
        if (h.flags & ~(SS_ATMOS_ACTIVE | SS_ATMOS_PROTECTED | SS_ATMOS_CONFLICT)) return "Unknown driver flags";
        if (h.flags & SS_ATMOS_PROTECTED) return "Protected audio: capture is unavailable";
        if (h.flags & SS_ATMOS_CONFLICT) return "Concurrent encoded streams: restart the source before capturing";
        if (h.dropped_bytes) return "Audio bytes were lost; this recording is not continuous";
        if (h.start_offset > h.produced_bytes || h.data_bytes > h.produced_bytes - h.start_offset) return "Invalid byte positions";
        if (started && h.generation != generation) return "The source format or capture session changed";
        if (h.data_bytes == 0) return nullptr;
        if (h.sample_rate != 192000 || h.channels != 8 || h.bits_per_sample != 16 || h.block_align != 16)
            return "The source is not the expected MAT carrier";
        if ((h.data_bytes % 16) || (h.start_offset % 16)) return "Unaligned carrier data";
        if (h.start_offset != next_offset) return "Missing or reordered carrier bytes";
        if (started && memcmp(subtype, h.subtype, sizeof(subtype))) return "The encoded subtype changed";
        if (!started) { started = true; generation = h.generation; memcpy(subtype, h.subtype, sizeof(subtype)); }
        next_offset += h.data_bytes;
        total_bytes += h.data_bytes;
        return nullptr;
    }
};
