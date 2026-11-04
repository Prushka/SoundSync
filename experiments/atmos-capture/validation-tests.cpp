#include <assert.h>
#include <stdio.h>
#include "capture-validation.h"

int main() {
    CaptureHeader h = {};
    h.version = SS_ATMOS_VERSION; h.header_bytes = sizeof(h); h.generation = 1;
    h.sample_rate = 192000; h.channels = 8; h.bits_per_sample = 16; h.block_align = 16;
    CaptureValidation v = {};
    assert(v.Accept(h, sizeof(h)) == nullptr && !v.started);
    h.data_bytes = 32; h.produced_bytes = 32;
    assert(v.Accept(h, sizeof(h) + 32) == nullptr && v.total_bytes == 32);
    h.start_offset = 32; h.produced_bytes = 64;
    assert(v.Accept(h, sizeof(h) + 32) == nullptr && v.total_bytes == 64);
    assert(v.Accept(h, sizeof(h) + 32) != nullptr); // Duplicate.
    h.start_offset = 80; h.produced_bytes = 112;
    assert(v.Accept(h, sizeof(h) + 32) != nullptr); // Gap.
    h.start_offset = 64; h.produced_bytes = 96;
    h.generation = 2;
    assert(v.Accept(h, sizeof(h) + 32) != nullptr); // New source, even with contiguous offsets.
    h.generation = 1; h.dropped_bytes = 1;
    assert(v.Accept(h, sizeof(h) + 32) != nullptr);
    h.dropped_bytes = 0; h.flags = SS_ATMOS_PROTECTED;
    assert(v.Accept(h, sizeof(h) + 32) != nullptr);
    h.flags = SS_ATMOS_CONFLICT;
    assert(v.Accept(h, sizeof(h) + 32) != nullptr);
    h.flags = 0; h.channels = 2;
    assert(v.Accept(h, sizeof(h) + 32) != nullptr);
    h.channels = 8;
    assert(v.Accept(h, sizeof(h) + 31) != nullptr);
    h.subtype[0] = 1;
    assert(v.Accept(h, sizeof(h) + 32) != nullptr);
    h.subtype[0] = 0;
    assert(v.Accept(h, sizeof(h) + 32) == nullptr && v.total_bytes == 96);
    h.start_offset = UINT64_MAX;
    assert(v.Accept(h, sizeof(h) + 32) != nullptr);
    puts("capture validation: gaps, duplicates, overflow, format changes, protected content and malformed responses passed");
}
