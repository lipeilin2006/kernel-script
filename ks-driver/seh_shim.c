/*
 * WDK/MSVC exception boundary.
 *
 * MmProbeAndLockPages raises a structured exception for invalid user pages.
 * Rust panic handling cannot catch SEH, and an SEH exception must not cross a
 * Rust ABI frame. Compile this file with the same WDK toolchain as the
 * driver and link it into the final .sys image.
 */
#include <ntifs.h>

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
