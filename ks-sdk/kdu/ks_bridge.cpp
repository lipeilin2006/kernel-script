/*******************************************************************************
*
*  TITLE:       KS_BRIDGE.CPP
*
*  In-process KDU map bridge for the KernelScript SDK.
*
*  The workspace compiles KDU's Hamakaze sources (minus main.cpp) into a
*  static library linked into ks-sdk; this file replaces the excluded
*  command-line entry with a direct-call entry:
*
*    ks_kdu_map(image, ...) -> NTSTATUS of DriverEntry (shellcode V3).
*
*  Both inputs are raw bytes held by the caller: the target driver image
*  and the packed providers database. Neither ever touches disk - the
*  image is mapped straight from memory by KsMapImageFromMemory instead of
*  KDU's supLoadFileForMapping (which maps a file through LdrLoadDll), so
*  the only files KDU writes are the helper drivers its provider and
*  victim callbacks extract. KsMapImageFromMemory reproduces what
*  LdrLoadDll produced for the old path: sections in place, base
*  relocations applied and OptionalHeader.ImageBase republished at the
*  actual base - the shellcode relocates the payload copy a second time
*  from that field, so it must match the base the image's absolute
*  pointers already sit at.
*
*  This file also presets the in-memory providers database (drv64.dll
*  bytes manually mapped the same way), replicates KDUMain's environment
*  preamble (HVCI and build number feed KDUProviderCreate) and ports
*  KDUProcessDrvMapSwitch with the file-based load replaced. ks_kdu_map
*  returns g_KduEntryStatus - the payload status KDUShowPayloadResult
*  recorded from the mapped shellcode section, the same value the old
*  child-process flow parsed from KDU's
*  "[~] Shellcode result: NTSTATUS (0x...)" line.
*
*******************************************************************************/

// Also /FI-included by build.rs; explicit include keeps the TU readable.
#include "ks_kdu_log.h"
#include "global.h"

#define T_PRNTDEFAULT "%s\r\n"

//
// Normally owned by the excluded main.cpp.
//
BOOL g_UseLA57 = FALSE;

//
// Log sink: printf_s/vprintf_s are macro-hooked to these in
// ks_kdu_log.h; ks-test streams the lines back through its step log.
//
static KsKduLogFn g_KsLogFn = NULL;

void __cdecl KsKduSetLog(KsKduLogFn fn)
{
    g_KsLogFn = fn;
}

int __cdecl KsKduPrintf(const char* fmt, ...)
{
    char buf[4096];
    va_list args;
    int length;

    va_start(args, fmt);
    length = vsnprintf(buf, sizeof(buf), fmt, args);
    va_end(args);

    if (length <= 0)
        return length;
    if ((size_t)length >= sizeof(buf)) {
        buf[sizeof(buf) - 1] = '\0';
        length = (int)sizeof(buf) - 1;
    }
    if (g_KsLogFn != NULL)
        g_KsLogFn(buf);
    return length;
}

int __cdecl KsKduVPrintf(const char* fmt, va_list args)
{
    char buf[4096];
    int length = vsnprintf(buf, sizeof(buf), fmt, args);

    if (length <= 0)
        return length;
    if ((size_t)length >= sizeof(buf)) {
        buf[sizeof(buf) - 1] = '\0';
        length = (int)sizeof(buf) - 1;
    }
    if (g_KsLogFn != NULL)
        g_KsLogFn(buf);
    return length;
}

//
// Map a raw PE image from memory: copy headers and sections, apply base
// relocations and republish OptionalHeader.ImageBase at the mapped base.
// Imports and TLS are intentionally left unresolved - KDU's payload copy
// resolves the kernel imports by name itself (supResolveKernelImport) and
// the loader never sees this image. The ImageBase rewrite mirrors what
// LdrLoadDll did for the previous file-based path: it is what the
// kernel-side shellcode computes its second relocation delta from
// (delta = exbuffer - ImageBase), so a header still naming the preferred
// base while the pointers sit at `base` would relocate the payload copy
// by the wrong amount.
//
static PVOID KsMapImageFromMemory(
    _In_reads_bytes_(RawSize) const BYTE* Raw,
    _In_ SIZE_T RawSize)
{
    const IMAGE_DOS_HEADER* dosHeader;
    const IMAGE_NT_HEADERS64* ntHeaders;
    const IMAGE_SECTION_HEADER* sections;
    const IMAGE_DATA_DIRECTORY* relocDir;
    BYTE* base;
    SIZE_T headersSize;
    SIZE_T imageSize;
    UINT i;

    if (Raw == NULL || RawSize < sizeof(IMAGE_DOS_HEADER))
        return NULL;

    dosHeader = (const IMAGE_DOS_HEADER*)Raw;
    if (dosHeader->e_magic != IMAGE_DOS_SIGNATURE ||
        (SIZE_T)dosHeader->e_lfanew + sizeof(IMAGE_NT_HEADERS64) > RawSize)
    {
        return NULL;
    }

    ntHeaders = (const IMAGE_NT_HEADERS64*)(Raw + dosHeader->e_lfanew);
    if (ntHeaders->Signature != IMAGE_NT_SIGNATURE ||
        ntHeaders->OptionalHeader.Magic != IMAGE_NT_OPTIONAL_HDR64_MAGIC)
    {
        return NULL;
    }

    imageSize = ntHeaders->OptionalHeader.SizeOfImage;
    headersSize = ntHeaders->OptionalHeader.SizeOfHeaders;
    if (imageSize == 0 || imageSize > (SIZE_T)256 * 1024 * 1024 ||
        headersSize == 0 || headersSize > RawSize || headersSize > imageSize ||
        (SIZE_T)dosHeader->e_lfanew + sizeof(IMAGE_NT_HEADERS64) > headersSize)
    {
        return NULL;
    }

    base = (BYTE*)VirtualAlloc(
        NULL,
        imageSize,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_EXECUTE_READWRITE);
    if (base == NULL)
        return NULL;

    RtlCopyMemory(base, Raw, headersSize);

    sections = IMAGE_FIRST_SECTION(ntHeaders);
    for (i = 0; i < ntHeaders->FileHeader.NumberOfSections; i++) {
        SIZE_T rawOffset = sections[i].PointerToRawData;
        SIZE_T rawBytes = sections[i].SizeOfRawData;

        if (rawBytes == 0)
            continue;
        if (rawOffset + rawBytes > RawSize ||
            (SIZE_T)sections[i].VirtualAddress + rawBytes > imageSize)
        {
            VirtualFree(base, 0, MEM_RELEASE);
            return NULL;
        }
        RtlCopyMemory(
            base + sections[i].VirtualAddress,
            Raw + rawOffset,
            rawBytes);
    }

    relocDir = &ntHeaders->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_BASERELOC];
    if (relocDir->Size != 0 && relocDir->VirtualAddress != 0) {
        LONG_PTR delta = (LONG_PTR)base - (LONG_PTR)ntHeaders->OptionalHeader.ImageBase;
        BYTE* relocBase;
        SIZE_T offset = 0;

        if ((SIZE_T)relocDir->VirtualAddress + relocDir->Size > imageSize) {
            VirtualFree(base, 0, MEM_RELEASE);
            return NULL;
        }

        relocBase = base + relocDir->VirtualAddress;
        while (offset + sizeof(IMAGE_BASE_RELOCATION) <= relocDir->Size) {
            const IMAGE_BASE_RELOCATION* block =
                (const IMAGE_BASE_RELOCATION*)(relocBase + offset);
            UINT count;
            const WORD* entries;
            UINT j;

            if (block->SizeOfBlock < sizeof(IMAGE_BASE_RELOCATION) ||
                offset + block->SizeOfBlock > relocDir->Size)
            {
                break;
            }

            count = (block->SizeOfBlock - sizeof(IMAGE_BASE_RELOCATION)) / sizeof(WORD);
            entries = (const WORD*)(block + 1);
            for (j = 0; j < count; j++) {
                UINT rva;

                switch (entries[j] >> 12) {
                case IMAGE_REL_BASED_ABSOLUTE:
                    break;
                case IMAGE_REL_BASED_DIR64:
                    rva = (UINT)block->VirtualAddress + (entries[j] & 0x0FFF);
                    if ((SIZE_T)rva + sizeof(ULONGLONG) > imageSize) {
                        VirtualFree(base, 0, MEM_RELEASE);
                        return NULL;
                    }
                    *(LONG_PTR*)(base + rva) += delta;
                    break;
                default:
                    // Both embedded images (drv64.dll, the target driver)
                    // are plain x64 PE32+ images; anything else means the
                    // bytes are not what we expect.
                    VirtualFree(base, 0, MEM_RELEASE);
                    return NULL;
                }
            }
            offset += block->SizeOfBlock;
        }

        // Absolute pointers now sit at `base`; republish it in the header
        // (LdrLoadDll did this too). An image without a relocation block
        // keeps the preferred base - its pointers are preferred-based and
        // rewriting the field would be the wrong direction.
        ((IMAGE_NT_HEADERS64*)(base + dosHeader->e_lfanew))->OptionalHeader.ImageBase =
            (ULONGLONG)(ULONG_PTR)base;
    }

    return base;
}

//
// GetProcAddress replacement for images that never went through the
// loader: on current Windows builds the kernel32/ntdll implementation
// rejects them (ERROR_MOD_NOT_FOUND). Pure PE export-directory walk.
//
static PVOID KsGetProcAddress(
    _In_ PVOID Base,
    _In_ const char* Name)
{
    BYTE* base = (BYTE*)Base;
    const IMAGE_DOS_HEADER* dosHeader;
    const IMAGE_NT_HEADERS64* ntHeaders;
    const IMAGE_DATA_DIRECTORY* exportDir;
    const IMAGE_EXPORT_DIRECTORY* exports;
    const DWORD* nameRvas;
    const WORD* nameOrdinals;
    const DWORD* functionRvas;
    DWORD i;

    if (base == NULL || Name == NULL)
        return NULL;

    dosHeader = (const IMAGE_DOS_HEADER*)base;
    if (dosHeader->e_magic != IMAGE_DOS_SIGNATURE)
        return NULL;
    ntHeaders = (const IMAGE_NT_HEADERS64*)(base + dosHeader->e_lfanew);
    if (ntHeaders->Signature != IMAGE_NT_SIGNATURE)
        return NULL;

    exportDir = &ntHeaders->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_EXPORT];
    if (exportDir->VirtualAddress == 0 || exportDir->Size == 0)
        return NULL;

    exports = (const IMAGE_EXPORT_DIRECTORY*)(base + exportDir->VirtualAddress);
    nameRvas = (const DWORD*)(base + exports->AddressOfNames);
    nameOrdinals = (const WORD*)(base + exports->AddressOfNameOrdinals);
    functionRvas = (const DWORD*)(base + exports->AddressOfFunctions);

    for (i = 0; i < exports->NumberOfNames; i++) {
        const char* exportName = (const char*)(base + nameRvas[i]);

        if (strcmp(exportName, Name) != 0)
            continue;

        {
            DWORD functionRva = functionRvas[nameOrdinals[i]];

            // Forwarded exports live inside the export directory itself;
            // drv64.dll has none, so treat them as not found.
            if (functionRva >= exportDir->VirtualAddress &&
                functionRva < exportDir->VirtualAddress + exportDir->Size)
            {
                return NULL;
            }
            return base + functionRva;
        }
    }

    return NULL;
}

//
// Port of KDUMain's environment preamble: the probe lines are kept
// verbatim for log parity and bHVCIRunning/NtBuildNumber feed
// KDUProcessDrvMapSwitch exactly like KDUProcessCommandLine received
// them. Returns 0 on success, an ERROR_* code otherwise.
//
static INT KsProbeEnvironment(
    _Out_ ULONG* HvciEnabled,
    _Out_ ULONG* NtBuildNumber)
{
    CHAR vendorString[0x20];
    OSVERSIONINFO osv;
    CHAR szVersion[100];
    BOOLEAN bSecureBoot;
    BOOLEAN bVBSRunning;
    BOOLEAN bHVCIRunning;
    BOOLEAN bHVCIStrict;
    SYSTEM_CODEINTEGRITY_INFORMATION ciPolicy;
    ULONG dummy = 0;
    BOOL hvciActive = FALSE;

    *HvciEnabled = 0;
    *NtBuildNumber = 0;

    RtlFillMemory(vendorString, sizeof(vendorString), 0);
    GET_CPU_VENDOR_STRING(vendorString);
    printf_s("[*] CPU vendor string: %s\r\n", vendorString);

    g_UseLA57 = supIsLA57Enabled();
    if (g_UseLA57) {
        printf_s("[*] LA57 enabled\r\n");
    }

    RtlSecureZeroMemory(&osv, sizeof(osv));
    osv.dwOSVersionInfoSize = sizeof(osv);
    RtlGetVersion((PRTL_OSVERSIONINFOW)&osv);
    if (osv.dwBuildNumber < NT_WIN7_RTM) {
        supPrintfEvent(kduEventError,
            "[!] Unsupported WinNT version\r\n");
        return ERROR_UNKNOWN_REVISION;
    }

    if (!ntsupUserIsFullAdmin()) {
        supPrintfEvent(kduEventError,
            "[!] Administrator privileges are required to continue.\r\n"
            "[!] Verify that you have sufficient privileges and you are not running program under any compatibility layer.\r\n");
        return ERROR_PRIVILEGE_NOT_HELD;
    }

    StringCchPrintfA(szVersion, 100,
        "[*] Windows version: %u.%u build %u",
        osv.dwMajorVersion,
        osv.dwMinorVersion,
        osv.dwBuildNumber);

    printf_s(T_PRNTDEFAULT, szVersion);

    if (supQuerySecureBootState(&bSecureBoot)) {
        printf_s("[*] SecureBoot is %sbled on this machine\r\n",
            bSecureBoot ? "ena" : "disa");
    }

    if (supQueryVBSState(&bVBSRunning, &bHVCIRunning, &bHVCIStrict)) {
        supPrintfEvent(kduEventInformation,
            "[*] Virtualization-based security: %s\r\n",
            bVBSRunning ? "Running" : "Not configured");
        supPrintfEvent(kduEventInformation,
            "[*] Hypervisor enforced Code Integrity running: %s\r\n",
            bHVCIRunning ? "Yes" : "Not configured");
    }

    RtlSecureZeroMemory(&ciPolicy, sizeof(ciPolicy));
    ciPolicy.Length = sizeof(ciPolicy);
    ciPolicy.CodeIntegrityOptions = 0;
    if (NT_SUCCESS(NtQuerySystemInformation(
        SystemCodeIntegrityInformation,
        &ciPolicy,
        sizeof(ciPolicy),
        &dummy)))
    {
        if (ciPolicy.CodeIntegrityOptions & CODEINTEGRITY_OPTION_TESTSIGN)
            printf_s("[*] Test Mode ENABLED\r\n");

        if (ciPolicy.CodeIntegrityOptions & CODEINTEGRITY_OPTION_DEBUGMODE_ENABLED)
            printf_s("[*] Debug Mode ENABLED\r\n");

        if (ciPolicy.CodeIntegrityOptions & CODEINTEGRITY_OPTION_HVCI_KMCI_ENABLED) {
            hvciActive = TRUE;
            printf_s("[*] HVCI KMCI ENABLED\r\n");
        }

        if (ciPolicy.CodeIntegrityOptions & CODEINTEGRITY_OPTION_WHQL_ENFORCEMENT_ENABLED)
            printf_s("[*] WHQL enforcement ENABLED\r\n");
    }

    if (osv.dwBuildNumber >= NT_WIN10_REDSTONE5) {
        BOOL bEnabled = FALSE;
        if (supDetectMsftBlockList(&bEnabled, FALSE, osv.dwBuildNumber, hvciActive)) {
            printf_s("[+] MSFT Driver block list is %sbled\r\n",
                (bEnabled) ? "ena" : "disa");
        }
    }

    *HvciEnabled = bHVCIRunning ? 1 : 0;
    *NtBuildNumber = osv.dwBuildNumber;
    return 0;
}

//
// Port of main.cpp's KDUProcessDrvMapSwitch (the excluded file), with the
// map-boundary calls kept verbatim and the file-based load replaced by an
// in-memory one: the target image arrives as bytes instead of a path, so
// nothing is validated against (or extracted from) the filesystem.
//
static INT KsProcessDrvMapSwitch(
    _In_ ULONG HvciEnabled,
    _In_ ULONG NtBuildNumber,
    _In_ ULONG ProviderId,
    _In_ ULONG ShellVersion,
    _In_reads_bytes_(DriverImageSize) const BYTE* DriverImage,
    _In_ SIZE_T DriverImageSize,
    _In_opt_ LPWSTR DriverObjectName,
    _In_opt_ LPWSTR DriverRegistryPath)
{
    INT retVal = 0;
    KDU_CONTEXT* provContext;
    PVOID pvImage = NULL;

#ifdef _DEBUG
    supPrintfEvent(kduEventError,
        "[!] Debug Mode run, shellcode is unavailable, abort.\r\n");
    return ERROR_INVALID_ENVIRONMENT;
#endif

    if (DriverImage == NULL || DriverImageSize == 0) {

        supPrintfEvent(kduEventError,
            "[!] Input driver image is empty, abort.\r\n");

        return 0;
    }

    printf_s("[*] Driver mapping using shellcode version: %lu\r\n", ShellVersion);

    if (ShellVersion == KDU_SHELLCODE_V3) {

        if (DriverObjectName == NULL) {

            supPrintfEvent(kduEventError,
                "[!] Driver object name is required when working with this shellcode\r\n"
                "[?] Use the following commands to supply object name and optionally registry key name\r\n"
                "\t-drvn [ObjectName] and/or\r\n"
                "\t-drvr [ObjectKeyName]\r\n"
                "\te.g. kdu -scv 3 -drvn MyName -map MyDriver.sys\r\n"
            );

            return 0;
        }
        else {
            printf_s("[+] Driver object name: \"%ws\"\r\n", DriverObjectName);
        }

        if (DriverRegistryPath) {
            printf_s("[+] Registry key name: \"%ws\"\r\n", DriverRegistryPath);
        }
        else {
            printf_s("[+] No driver registry key name specified, driver object name will be used instead\r\n");
        }

    }

    pvImage = KsMapImageFromMemory(DriverImage, DriverImageSize);

    if (pvImage == NULL) {
        supPrintfEvent(kduEventError,
            "[!] Error while mapping input driver image\r\n");
        return 0;
    }
    else {
        printf_s("[+] Input driver image mapped at 0x%p\r\n", pvImage);

        provContext = KDUProviderCreate(ProviderId,
            HvciEnabled,
            NtBuildNumber,
            ShellVersion,
            ActionTypeMapDriver);

        if (provContext) {

            if (ShellVersion == KDU_SHELLCODE_V3) {

                if (DriverObjectName) {
                    ScCreateFixedUnicodeString(&provContext->DriverObjectName,
                        DriverObjectName);

                }

                //
                // Registry path name is optional.
                // If not specified we will assume its the same name as driver object.
                //
                if (DriverRegistryPath) {
                    ScCreateFixedUnicodeString(&provContext->DriverRegistryPath,
                        DriverRegistryPath);
                }

            }

            retVal = provContext->Provider->Callbacks.MapDriver(provContext, pvImage);
            KDUProviderRelease(provContext);
        }

        VirtualFree(pvImage, 0, MEM_RELEASE);
    }

    return retVal;
}

extern "C" ULONG __cdecl ks_kdu_map(
    _In_reads_bytes_(DriverImageSize) const BYTE* DriverImage,
    _In_ SIZE_T DriverImageSize,
    _In_opt_ const WCHAR* DriverObjectName,
    _In_opt_ const WCHAR* DriverRegistryPath,
    _In_ ULONG ProviderId,
    _In_ ULONG ShellVersion,
    _In_reads_bytes_opt_(DbImageSize) const BYTE* DbImage,
    _In_ SIZE_T DbImageSize,
    _In_ KsKduLogFn LogFn)
{
    static BOOLEAN heapPolicyApplied = FALSE;
    ULONG hvciEnabled = 0;
    ULONG ntBuildNumber = 0;
    INT probeResult;
    INT retVal;

    if (!heapPolicyApplied) {
        HeapSetInformation(NULL, HeapEnableTerminationOnCorruption, NULL, 0);
        heapPolicyApplied = TRUE;
    }

    g_KsLogFn = LogFn;

    if (DriverImage == NULL || DriverImageSize == 0 || ShellVersion == 0) {
        supPrintfEvent(kduEventError, "[!] Invalid map arguments\r\n");
        return STATUS_INVALID_PARAMETER;
    }

    //
    // One-time database preset: map the embedded drv64.dll bytes and
    // validate them exactly like KDUProviderLoadExternalDb would.
    // KDUProviderLoadDB short-circuits on the preset globals afterwards.
    //
    if (g_KduDbModule == NULL) {
        PVOID dbBase;

        if (DbImage == NULL || DbImageSize == 0) {
            supPrintfEvent(kduEventError,
                "[!] Embedded drivers database not provided\r\n");
            return STATUS_INVALID_PARAMETER;
        }

        dbBase = KsMapImageFromMemory(DbImage, DbImageSize);
        if (dbBase == NULL) {
            supPrintfEvent(kduEventError,
                "[!] Cannot map embedded drivers database\r\n");
            return STATUS_INVALID_IMAGE_FORMAT;
        }

        {
            PVOID procVersion = KsGetProcAddress(dbBase, "gVersion");
            PVOID procTable = KsGetProcAddress(dbBase, "gProvTable");

            if (procVersion == NULL || procTable == NULL) {
                supPrintfEvent(kduEventError,
                    "[!] Drivers database exports not found\r\n");
                return STATUS_INVALID_IMAGE_FORMAT;
            }

            if (KDUProviderSetPresetDb(
                (HINSTANCE)dbBase,
                procVersion,
                procTable) == NULL)
            {
                return STATUS_INVALID_IMAGE_FORMAT;
            }
        }
    }

    probeResult = KsProbeEnvironment(&hvciEnabled, &ntBuildNumber);
    if (probeResult != 0) {
        return STATUS_UNSUCCESSFUL;
    }

    printf_s("[+] Selected provider: %lu\r\n", ProviderId);

    //
    // The payload status is the authoritative result: reset it first,
    // then run the ported map switch. KDUShowPayloadResult records the
    // shellcode section status (DriverEntry's own under V3).
    //
    g_KduEntryStatus = 0xFFFFFFFF;

    retVal = KsProcessDrvMapSwitch(
        hvciEnabled,
        ntBuildNumber,
        ProviderId,
        ShellVersion,
        DriverImage,
        DriverImageSize,
        (LPWSTR)DriverObjectName,
        (LPWSTR)DriverRegistryPath);
    (void)retVal;

    if (g_KduEntryStatus != 0xFFFFFFFF) {
        return g_KduEntryStatus;
    }

    //
    // No payload result: the attempt died before the shellcode section
    // existed (provider abort, victim load failure, image error).
    //
    return STATUS_UNSUCCESSFUL;
}
