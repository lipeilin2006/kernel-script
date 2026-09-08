/*
 * WDK/MSVC exception boundary.
 *
 * MmProbeAndLockPages raises a structured exception for invalid user pages.
 * Rust panic handling cannot catch SEH, and an SEH exception must not cross a
 * Rust ABI frame. Compile this file with the same WDK toolchain as the
 * driver and link it into the final .sys image.
 */
#include <ntddk.h>

static const GUID KS_DEVICE_CLASS_GUID = {
    0x7d7f1e42, 0x3c5f, 0x4c3d,
    { 0x9a, 0x81, 0x4d, 0x6e, 0x3b, 0x5f, 0x19, 0x72 }
};

static PEPROCESS g_trusted_process = NULL;

NTSTATUS ks_authorize_device_request(PIRP irp)
{
    PIO_STACK_LOCATION stack = IoGetCurrentIrpStackLocation(irp);
    PEPROCESS current = PsGetCurrentProcess();

    if (stack->MajorFunction == IRP_MJ_CREATE) {
        if (InterlockedCompareExchangePointer(
                (PVOID volatile *)&g_trusted_process, current, NULL) == NULL) {
            ObReferenceObject(current);
            return STATUS_SUCCESS;
        }
        return g_trusted_process == current ? STATUS_SUCCESS : STATUS_ACCESS_DENIED;
    }

    if (stack->MajorFunction == IRP_MJ_CLOSE) {
        if (g_trusted_process == current &&
            InterlockedCompareExchangePointer(
                (PVOID volatile *)&g_trusted_process, NULL, current) == current) {
            ObDereferenceObject(current);
        }
        return STATUS_SUCCESS;
    }

    return g_trusted_process == current ? STATUS_SUCCESS : STATUS_ACCESS_DENIED;
}

NTSTATUS ks_create_secure_device(
    PDRIVER_OBJECT driver,
    PUNICODE_STRING device_name,
    PDEVICE_OBJECT *device
)
{
    // Only LocalSystem may open the device. The service runs as SYSTEM; an
    // administrator token is intentionally not granted direct device access.
    UNICODE_STRING sddl = RTL_CONSTANT_STRING(L"D:P(A;;GA;;;SY)");
    return WdmlibIoCreateDeviceSecure(
        driver, 0, device_name, FILE_DEVICE_UNKNOWN,
        FILE_DEVICE_SECURE_OPEN, TRUE, &sddl,
        (LPGUID)&KS_DEVICE_CLASS_GUID, device
    );
}

NTSTATUS ks_probe_and_lock_pages(
    PMDL mdl,
    KPROCESSOR_MODE mode,
    LOCK_OPERATION access
)
{
    __try {
        MmProbeAndLockPages(mdl, mode, access);
        return STATUS_SUCCESS;
    }
    __except (EXCEPTION_EXECUTE_HANDLER) {
        return (NTSTATUS)GetExceptionCode();
    }
}

/* These WDK helpers are macros/inline definitions, not linkable exports. */
PIO_STACK_LOCATION ks_get_current_irp_stack_location(PIRP irp)
{
    return IoGetCurrentIrpStackLocation(irp);
}

ULONG ks_get_ioctl_code(PIRP irp)
{
    return IoGetCurrentIrpStackLocation(irp)->Parameters.DeviceIoControl.IoControlCode;
}

ULONG ks_get_input_buffer_length(PIRP irp)
{
    return IoGetCurrentIrpStackLocation(irp)->Parameters.DeviceIoControl.InputBufferLength;
}

ULONG ks_get_output_buffer_length(PIRP irp)
{
    return IoGetCurrentIrpStackLocation(irp)->Parameters.DeviceIoControl.OutputBufferLength;
}

PVOID ks_get_system_buffer(PIRP irp)
{
    return irp->AssociatedIrp.SystemBuffer;
}

NTSTATUS ks_copy_process_memory(
    PEPROCESS source_process,
    PVOID source_address,
    PVOID target_address,
    SIZE_T size,
    PSIZE_T copied
)
{
    return MmCopyVirtualMemory(
        source_process,
        source_address,
        PsGetCurrentProcess(),
        target_address,
        size,
        KernelMode,
        copied
    );
}

NTSTATUS ks_write_process_memory(
    PEPROCESS target_process,
    PVOID source_address,
    PVOID target_address,
    SIZE_T size,
    PSIZE_T copied
)
{
    return MmCopyVirtualMemory(
        PsGetCurrentProcess(),
        source_address,
        target_process,
        target_address,
        size,
        KernelMode,
        copied
    );
}

PVOID ks_get_system_address_for_mdl_safe(PMDL mdl, ULONG priority)
{
    return MmGetSystemAddressForMdlSafe(mdl, priority);
}

void ks_complete_irp(PIRP irp, NTSTATUS status, ULONG_PTR information, CCHAR priority)
{
    irp->IoStatus.Status = status;
    irp->IoStatus.Information = information;
    IofCompleteRequest(irp, priority);
}

/*
 * Rust's MSVC target may retain this personality symbol in libcore even when
 * the driver is built with panic=abort. The kernel image must not use the user
 * mode C++ runtime, so provide only the ABI-compatible fail-closed fallback.
 * The normal MmProbeAndLockPages SEH path is handled above and never reaches
 * this function.
 */
int _fltused = 0x9876;

EXCEPTION_DISPOSITION __CxxFrameHandler3(
    PEXCEPTION_RECORD exception_record,
    ULONG64 establisher_frame,
    PCONTEXT context_record,
    PVOID dispatcher_context
)
{
    UNREFERENCED_PARAMETER(exception_record);
    UNREFERENCED_PARAMETER(establisher_frame);
    UNREFERENCED_PARAMETER(context_record);
    UNREFERENCED_PARAMETER(dispatcher_context);
    return ExceptionContinueSearch;
}
