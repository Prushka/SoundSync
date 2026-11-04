#include <assert.h>
#include <stdio.h>
#include "capture-buffer.h"

int main() {
    unsigned char storage[16] = {}, output[32] = {};
    const unsigned char input[] = {0x72, 0xf8, 0x1f, 0x4e, 0x16, 0, 0, 0, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19};
    const void* stream = input;
    CaptureBuffer b = {};
    b.Initialize(storage, sizeof(storage));
    b.Begin(stream);
    b.Write(stream, input, 8);
    assert(b.ring.written == 0); // No capture without an open reader.
    b.Open();
    b.Write(stream, input, 12);
    assert(b.ring.Read(output, 8) == 8 && memcmp(output, input, 8) == 0);
    b.Write(stream, input + 12, 8); // Wrap across the end of the allocation.
    assert(b.ring.Read(output, 32) == 12 && memcmp(output, input + 8, 12) == 0);
    assert(b.ring.written == 20 && b.ring.consumed == 20 && b.ring.dropped == 0);
    b.Write(stream, input, 20); // A single chunk larger than capacity keeps its tail.
    assert(b.ring.dropped == 4 && b.ring.consumed == 24 && b.ring.written == 40);
    assert(b.ring.Read(output, 16) == 16 && memcmp(output, input + 4, 16) == 0);
    b.Write(stream, input, 12);
    b.Write(stream, input + 12, 8); // Overflow with previously unread data.
    assert(b.ring.dropped == 8 && b.ring.consumed == 44);
    assert(b.ring.Read(output, 16) == 16 && memcmp(output, input + 4, 16) == 0);
    b.Write(stream, input, 4);
    b.Gap(stream, 7); // The DMA clock advanced beyond retained audio.
    assert(b.ring.written == 71 && b.ring.consumed == 71 && b.ring.dropped == 19);
    b.Write(stream, input, 8);
    const auto before_protection = b.generation;
    b.Protect(stream, true);
    b.Write(stream, input, 8);
    assert(b.generation > before_protection && b.ring.Read(output, 32) == 0 && b.ring.written == 0);
    b.Protect(stream, false);
    b.Write(stream, input, 8);
    assert(b.ring.Read(output, 32) == 8);
    assert(!b.Begin(output)); // Never merge two independent encoded streams.
    b.Write(output, input, 8);
    assert(b.conflict && b.ring.Read(output, 32) == 0);
    b.End(output);
    assert(b.owner == stream);
    b.Write(stream, input, 4);
    b.End(stream);
    assert(b.owner == nullptr && b.ring.Read(output, 32) == 4); // Drain a stopped stream.
    assert(b.Begin(output) && !b.conflict && b.ring.written == 0);
    b.Write(output, input, 8);
    b.Close();
    assert(!b.client && b.ring.Read(output, 32) == 0);
    b.Write(output, input, 8);
    assert(b.ring.written == 0);
    puts("capture buffer: byte fidelity, wrap, overflow, DMA gaps, ownership, reader lifetime and protected audio passed");
}
