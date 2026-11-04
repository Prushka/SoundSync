#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <stdio.h>
#include <wchar.h>
#include <stdlib.h>
#include "capture-validation.h"

static void Usage() {
    puts("SoundSync experimental MAT capture reader (never plays audio)\n"
         "  capture-reader.exe --status\n"
         "  capture-reader.exe --capture output.ssatm SECONDS\n"
         "Requires the separately installed probe driver and an elevated terminal.\n"
         "The SSATMOS1 file holds 96-byte headers followed by unchanged carrier bytes.\n"
         "An encoded stream or MAT burst alone does not prove Atmos object metadata.");
}

static bool WriteAll(HANDLE file, const void* data, DWORD bytes) {
    DWORD written = 0;
    return WriteFile(file, data, bytes, &written, nullptr) && written == bytes;
}

int wmain(int argc, wchar_t** argv) {
    if (argc == 2 && wcscmp(argv[1], L"--help") == 0) { Usage(); return 0; }
    const bool status_only = argc == 2 && wcscmp(argv[1], L"--status") == 0;
    if (!status_only && !(argc == 4 && wcscmp(argv[1], L"--capture") == 0)) { Usage(); return 2; }
    wchar_t* end = nullptr;
    const long seconds = status_only ? 0 : wcstol(argv[3], &end, 10);
    if (!status_only && (seconds < 1 || seconds > 300 || end == argv[3] || *end)) {
        fputs("SECONDS must be between 1 and 300.\n", stderr); return 2;
    }
    HANDLE device = CreateFileW(L"\\\\.\\SoundSyncAtmosProbe", GENERIC_READ, 0, nullptr, OPEN_EXISTING, 0, nullptr);
    if (device == INVALID_HANDLE_VALUE) {
        fprintf(stderr, "Cannot open probe driver (Windows error %lu). It may be absent, busy, or require elevation.\n", GetLastError());
        return 3;
    }
    HANDLE file = INVALID_HANDLE_VALUE;
    if (!status_only) {
        file = CreateFileW(argv[2], GENERIC_WRITE, 0, nullptr, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, nullptr);
        if (file == INVALID_HANDLE_VALUE || !WriteAll(file, "SSATMOS1", 8)) {
            fprintf(stderr, "Cannot create a new capture file (Windows error %lu). Existing files are never overwritten.\n", GetLastError());
            if (file != INVALID_HANDLE_VALUE) CloseHandle(file);
            CloseHandle(device); return 4;
        }
    }
    alignas(CaptureHeader) unsigned char bytes[sizeof(CaptureHeader) + SS_ATMOS_MAX_READ];
    const ULONGLONG deadline = GetTickCount64() + static_cast<ULONGLONG>(seconds) * 1000;
    CaptureValidation validation = {};
    uint64_t mat_bursts = 0;
    int result = 0;
    do {
        DWORD returned = 0;
        if (!DeviceIoControl(device, SS_ATMOS_READ, nullptr, 0, bytes, sizeof(bytes), &returned, nullptr)) {
            fprintf(stderr, "Driver read failed (Windows error %lu).\n", GetLastError()); result = 5; break;
        }
        if (returned < sizeof(CaptureHeader)) { fputs("Truncated driver response.\n", stderr); result = 5; break; }
        const auto& h = *reinterpret_cast<const CaptureHeader*>(bytes);
        const char* error = validation.Accept(h, returned);
        if (error) { fprintf(stderr, "%s\n", error); result = 6; break; }
        if (status_only) {
            printf("encoded_stream=%s, carrier=%u Hz/%u channels/%u bits, generation=%llu, dropped=%llu\n",
                h.flags & SS_ATMOS_ACTIVE ? "active" : "inactive", h.sample_rate, h.channels, h.bits_per_sample, h.generation, h.dropped_bytes);
            break;
        }
        if (h.data_bytes) {
            if (!WriteAll(file, bytes, returned)) { fputs("Capture file write failed.\n", stderr); result = 7; break; }
            const auto data = bytes + sizeof(CaptureHeader);
            for (size_t i = 0; i + 8 <= h.data_bytes; i += 2) {
                if (data[i] == 0x72 && data[i + 1] == 0xf8 && data[i + 2] == 0x1f && data[i + 3] == 0x4e &&
                    (data[i + 4] & 0x1f) == 0x16) ++mat_bursts;
            }
        } else {
            Sleep(2);
        }
    } while (GetTickCount64() < deadline);
    CloseHandle(device);
    if (file != INVALID_HANDLE_VALUE) {
        if (!FlushFileBuffers(file) && result == 0) result = 7;
        CloseHandle(file);
        printf("Captured %llu carrier bytes; observed %llu MAT/MLP burst preambles.\n", validation.total_bytes, mat_bursts);
        if (result == 0 && (validation.total_bytes < 3072000 || mat_bursts == 0)) {
            fputs("No sustained MAT capture was established. Do not treat this file as verified Atmos.\n", stderr); result = 8;
        }
        if (result == 0) puts("Continuous encoded capture obtained. Atmos decoding and receiver playback still require verification.");
        else fputs("Capture is incomplete or invalid; the diagnostic file was retained.\n", stderr);
    }
    return result;
}
