// SoundSync experimental capture interface for Microsoft's MS-PL-licensed SysVAD sample.
#include <sysvad.h>
#include <wdmsec.h>
#include "ss-atmos-capture.h"
#include "capture-buffer.h"
#include "capture-protocol.h"

static constexpr ULONG PoolTag = 'ASSS';
static constexpr size_t Capacity = 4 * 1024 * 1024;
static PDEVICE_OBJECT ControlDevice;
static PDRIVER_DISPATCH OriginalDispatch[IRP_MJ_MAXIMUM_FUNCTION + 1];
static KSPIN_LOCK CaptureLock;
static CaptureBuffer Buffer;
static CaptureHeader Format;
static UNICODE_STRING DeviceName = RTL_CONSTANT_STRING(L"\\Device\\SoundSyncAtmosProbe");
static UNICODE_STRING LinkName = RTL_CONSTANT_STRING(L"\\DosDevices\\SoundSyncAtmosProbe");
static const GUID ControlClass = {0x12cd8b22, 0x2379, 0x4ded, {0x87, 0x13, 0xa0, 0x9b, 0x49, 0xe2, 0x52, 0x44}};

static NTSTATUS Complete(PIRP irp, NTSTATUS status, ULONG_PTR bytes = 0) {
    irp->IoStatus.Status = status;
    irp->IoStatus.Information = bytes;
    IoCompleteRequest(irp, IO_NO_INCREMENT);
    return status;
}

static NTSTATUS Dispatch(PDEVICE_OBJECT device, PIRP irp) {
    const PIO_STACK_LOCATION stack = IoGetCurrentIrpStackLocation(irp);
    if (device != ControlDevice) return OriginalDispatch[stack->MajorFunction](device, irp);
    if (stack->FileObject == nullptr) return Complete(irp, STATUS_INVALID_DEVICE_REQUEST);
    KIRQL oldIrql;
    if (stack->MajorFunction == IRP_MJ_CREATE) {
        if (stack->FileObject->FileName.Length != 0) return Complete(irp, STATUS_OBJECT_NAME_NOT_FOUND);
        KeAcquireSpinLock(&CaptureLock, &oldIrql);
        const bool busy = Buffer.client;
        if (!busy) Buffer.Open();
        KeReleaseSpinLock(&CaptureLock, oldIrql);
        if (!busy) stack->FileObject->FsContext = device;
        return Complete(irp, busy ? STATUS_SHARING_VIOLATION : STATUS_SUCCESS);
    }
    if (stack->MajorFunction == IRP_MJ_CLEANUP || stack->MajorFunction == IRP_MJ_CLOSE) {
        if (stack->FileObject->FsContext == device) {
            KeAcquireSpinLock(&CaptureLock, &oldIrql);
            Buffer.Close();
            KeReleaseSpinLock(&CaptureLock, oldIrql);
            stack->FileObject->FsContext = nullptr;
        }
        return Complete(irp, STATUS_SUCCESS);
    }
    if (stack->MajorFunction != IRP_MJ_DEVICE_CONTROL || stack->Parameters.DeviceIoControl.IoControlCode != SS_ATMOS_READ)
        return Complete(irp, STATUS_INVALID_DEVICE_REQUEST);
    if (stack->FileObject->FsContext != device) return Complete(irp, STATUS_INVALID_HANDLE);
    const ULONG length = stack->Parameters.DeviceIoControl.OutputBufferLength;
    if (stack->Parameters.DeviceIoControl.InputBufferLength != 0 || length < sizeof(CaptureHeader) ||
        length > sizeof(CaptureHeader) + SS_ATMOS_MAX_READ || irp->AssociatedIrp.SystemBuffer == nullptr)
        return Complete(irp, STATUS_INVALID_PARAMETER);
    auto header = static_cast<CaptureHeader*>(irp->AssociatedIrp.SystemBuffer);
    KeAcquireSpinLock(&CaptureLock, &oldIrql);
    *header = Format;
    header->version = SS_ATMOS_VERSION;
    header->header_bytes = sizeof(CaptureHeader);
    header->generation = Buffer.generation;
    header->start_offset = Buffer.ring.consumed;
    header->produced_bytes = Buffer.ring.written;
    header->dropped_bytes = Buffer.ring.dropped;
    header->flags = (Buffer.owner ? SS_ATMOS_ACTIVE : 0) | (Buffer.protected_content ? SS_ATMOS_PROTECTED : 0) |
        (Buffer.conflict ? SS_ATMOS_CONFLICT : 0);
    header->data_bytes = static_cast<uint32_t>(Buffer.ring.Read(reinterpret_cast<unsigned char*>(header + 1), length - sizeof(*header)));
    KeReleaseSpinLock(&CaptureLock, oldIrql);
    return Complete(irp, STATUS_SUCCESS, sizeof(*header) + header->data_bytes);
}

NTSTATUS SsAtmosInitialize(PDRIVER_OBJECT driver) {
    KeInitializeSpinLock(&CaptureLock);
    auto storage = static_cast<unsigned char*>(ExAllocatePool2(POOL_FLAG_NON_PAGED, Capacity, PoolTag));
    if (storage == nullptr) return STATUS_INSUFFICIENT_RESOURCES;
    Buffer.Initialize(storage, Capacity);
    LARGE_INTEGER frequency;
    KeQueryPerformanceCounter(&frequency);
    Format.qpc_frequency = frequency.QuadPart;
    UNICODE_STRING sddl = RTL_CONSTANT_STRING(L"D:P(A;;GA;;;SY)(A;;GA;;;BA)");
    NTSTATUS status = IoCreateDeviceSecure(driver, 0, &DeviceName, 0x8337, FILE_DEVICE_SECURE_OPEN, TRUE, &sddl, &ControlClass, &ControlDevice);
    if (!NT_SUCCESS(status)) { ExFreePoolWithTag(storage, PoolTag); Buffer.ring.data = nullptr; return status; }
    status = IoCreateSymbolicLink(&LinkName, &DeviceName);
    if (!NT_SUCCESS(status)) {
        IoDeleteDevice(ControlDevice); ControlDevice = nullptr;
        ExFreePoolWithTag(storage, PoolTag); Buffer.ring.data = nullptr; return status;
    }
    // Reject every unsupported request to the control device here. Passing one
    // to PortCls would treat its empty extension as an audio-adapter extension.
    for (ULONG code = 0; code <= IRP_MJ_MAXIMUM_FUNCTION; ++code) {
        OriginalDispatch[code] = driver->MajorFunction[code]; driver->MajorFunction[code] = Dispatch;
    }
    ControlDevice->Flags |= DO_BUFFERED_IO;
    ControlDevice->Flags &= ~DO_DEVICE_INITIALIZING;
    return STATUS_SUCCESS;
}

void SsAtmosDestroy() {
    if (ControlDevice) {
        auto driver = ControlDevice->DriverObject;
        for (ULONG code = 0; code <= IRP_MJ_MAXIMUM_FUNCTION; ++code) driver->MajorFunction[code] = OriginalDispatch[code];
        IoDeleteSymbolicLink(&LinkName);
        IoDeleteDevice(ControlDevice);
        ControlDevice = nullptr;
    }
    if (Buffer.ring.data) { ExFreePoolWithTag(Buffer.ring.data, PoolTag); Buffer.ring.data = nullptr; }
}

void SsAtmosBegin(const void* stream, const WAVEFORMATEXTENSIBLE* format) {
    if (!format || format->Format.wFormatTag != WAVE_FORMAT_EXTENSIBLE || format->Format.cbSize < 22 ||
        format->Format.nChannels != 8 || format->Format.nSamplesPerSec != 192000 ||
        format->Format.wBitsPerSample != 16 || format->Format.nBlockAlign != 16) return;
    if (!IsEqualGUID(format->SubFormat, KSDATAFORMAT_SUBTYPE_IEC61937_DOLBY_MLP) &&
        !IsEqualGUID(format->SubFormat, KSDATAFORMAT_SUBTYPE_IEC61937_DOLBY_MAT20) &&
        !IsEqualGUID(format->SubFormat, KSDATAFORMAT_SUBTYPE_IEC61937_DOLBY_MAT21)) return;
    KIRQL oldIrql;
    KeAcquireSpinLock(&CaptureLock, &oldIrql);
    if (Buffer.Begin(stream)) {
        Format.sample_rate = format->Format.nSamplesPerSec;
        Format.channels = format->Format.nChannels;
        Format.bits_per_sample = format->Format.wBitsPerSample;
        Format.block_align = format->Format.nBlockAlign;
        RtlCopyMemory(Format.subtype, &format->SubFormat, sizeof(Format.subtype));
        Format.last_write_qpc = 0;
    }
    KeReleaseSpinLock(&CaptureLock, oldIrql);
}

void SsAtmosEnd(const void* stream) {
    KIRQL oldIrql;
    KeAcquireSpinLock(&CaptureLock, &oldIrql); Buffer.End(stream); KeReleaseSpinLock(&CaptureLock, oldIrql);
}

void SsAtmosProtect(const void* stream, bool protect) {
    KIRQL oldIrql;
    KeAcquireSpinLock(&CaptureLock, &oldIrql); Buffer.Protect(stream, protect); KeReleaseSpinLock(&CaptureLock, oldIrql);
}

void SsAtmosWrite(const void* stream, const unsigned char* data, ULONG bytes) {
    KIRQL oldIrql;
    KeAcquireSpinLock(&CaptureLock, &oldIrql);
    const auto previous = Buffer.ring.written;
    Buffer.Write(stream, data, bytes);
    if (Buffer.ring.written != previous) Format.last_write_qpc = KeQueryPerformanceCounter(nullptr).QuadPart;
    KeReleaseSpinLock(&CaptureLock, oldIrql);
}

void SsAtmosGap(const void* stream, ULONG bytes) {
    KIRQL oldIrql;
    KeAcquireSpinLock(&CaptureLock, &oldIrql); Buffer.Gap(stream, bytes); KeReleaseSpinLock(&CaptureLock, oldIrql);
}
