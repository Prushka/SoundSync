#pragma once
#include "capture-types.h"
#include <stddef.h>
#include <string.h>

struct ByteRing {
    unsigned char* data;
    size_t capacity;
    uint64_t written, consumed, dropped;
    void Reset() { written = consumed = dropped = 0; }
    void Write(const unsigned char* source, size_t count) {
        const uint64_t end = written + count;
        const uint64_t oldest = end > capacity ? end - capacity : 0;
        if (oldest > consumed) { dropped += oldest - consumed; consumed = oldest; }
        if (count > capacity) { source += count - capacity; count = capacity; }
        const size_t offset = static_cast<size_t>((end - count) % capacity);
        const size_t first = count < capacity - offset ? count : capacity - offset;
        memcpy(data + offset, source, first);
        memcpy(data, source + first, count - first);
        written = end;
    }
    size_t Read(unsigned char* destination, size_t limit) {
        const size_t available = static_cast<size_t>(written - consumed);
        const size_t count = available < limit ? available : limit;
        const size_t offset = static_cast<size_t>(consumed % capacity);
        const size_t first = count < capacity - offset ? count : capacity - offset;
        memcpy(destination, data + offset, first);
        memcpy(destination + first, data, count - first);
        consumed += count;
        return count;
    }
    void Gap(size_t count) {
        dropped += written - consumed + count;
        written += count;
        consumed = written;
    }
};
// The caller serializes every method and snapshot with a spin lock in kernel mode.
// This class owns no memory and is also compiled into the user-mode regression tests.
struct CaptureBuffer {
    ByteRing ring;
    uint64_t generation;
    const void* owner;
    bool client, conflict, protected_content;
    void Initialize(unsigned char* storage, size_t capacity) {
        ring.data = storage; ring.capacity = capacity;
        owner = nullptr; client = conflict = protected_content = false; generation = 0;
        Reset();
    }
    void Reset() { ring.Reset(); ++generation; }
    bool Begin(const void* stream) {
        if (owner != nullptr) { if (owner != stream) conflict = true; return false; }
        owner = stream; conflict = protected_content = false; Reset(); return true;
    }
    void End(const void* stream) { if (owner == stream) owner = nullptr; }
    void Open() { client = true; Reset(); }
    void Close() { client = false; Reset(); }
    void Write(const void* stream, const unsigned char* source, size_t count) {
        if (client && owner == stream && !protected_content) ring.Write(source, count);
    }
    void Gap(const void* stream, size_t count) {
        if (client && owner == stream && !protected_content) ring.Gap(count);
    }
    void Protect(const void* stream, bool protect) {
        if (owner == stream && protected_content != protect) { protected_content = protect; Reset(); }
    }
};
