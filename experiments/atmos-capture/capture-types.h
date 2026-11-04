#pragma once
#ifdef _KERNEL_MODE
using uint8_t = UCHAR;
using uint32_t = ULONG;
using uint64_t = ULONGLONG;
#else
#include <stdint.h>
#endif
