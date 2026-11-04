#pragma once
// Included after sysvad.h. All audio callbacks are nonpaged and allocation-free.
NTSTATUS SsAtmosInitialize(PDRIVER_OBJECT driver);
void SsAtmosDestroy();
void SsAtmosBegin(const void* stream, const WAVEFORMATEXTENSIBLE* format);
void SsAtmosEnd(const void* stream);
void SsAtmosProtect(const void* stream, bool protect);
void SsAtmosWrite(const void* stream, const unsigned char* data, ULONG bytes);
void SsAtmosGap(const void* stream, ULONG bytes);
