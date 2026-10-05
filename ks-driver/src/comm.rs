//! Shared-memory transport: one named section plus a request/response event
//! pair, serviced by a single system thread that also replays the memory
//! lock table between requests (no dedicated rewrite thread).
//!
//! The driver creates (or reopens) all three objects with a DACL that grants
//! `GENERIC_ALL` to LocalSystem and the Administrators group, maps the
//! section into system space, and runs [`worker`] until unload. User-mode
//! clients reach the same objects through the `Global\` prefix.
//!
//! The three object names are randomized at every startup: a 16-hex-char
//! token (seeded from interrupt time, drawn with `RtlRandomEx`) is appended
//! to fixed prefixes, and the full names are published under
//! `HKLM\SOFTWARE\KernelScript` — clients resolve them from there at
//! session open. A reload therefore creates fresh objects under fresh
//! names; a client still holding the previous load's objects is orphaned.
//! Teardown ([`release_instance`]) erases the whole publication again, so
//! after a clean exit the key is gone and only a crash leaves values
//! behind for the next load to overwrite.
//!
//! Single instance: before the ring exists, [`claim_instance`] takes a
//! kernel-only marker event, so a second load (SCM or a manual mapper) is
//! refused with `STATUS_OBJECT_NAME_COLLISION` instead of splitting the
//! ring and the lock table across two driver images. The marker name itself
//! stays fixed — it is the liveness probe both loads must agree on.

use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::request;
use crate::wdm::*;
use ks_core::ring::{
    header, request_bytes, response_bytes, RING_TOTAL_SIZE, STATE_PROCESSING, STATE_REQUEST,
};

/// Longest object name handled here (`\BaseNamedObjects\...` is well under
/// this), including the terminating NUL.
const OBJECT_NAME_CAPACITY: usize = 64;
/// Fixed prefix of the section name; [`RingNames`] appends `-{token}`.
const SECTION_NAME_PREFIX: &str = "\\BaseNamedObjects\\KernelScriptSection-";
/// Fixed prefix of the request-event name; [`RingNames`] appends `-{token}`.
const REQUEST_NAME_PREFIX: &str = "\\BaseNamedObjects\\KernelScriptRequest-";
/// Fixed prefix of the response-event name; [`RingNames`] appends `-{token}`.
const RESPONSE_NAME_PREFIX: &str = "\\BaseNamedObjects\\KernelScriptResponse-";
/// Section DACL: `SECURITY_DESCRIPTOR` header plus room for two ACEs.
const DACL_CAPACITY: usize = 128;
/// Handle access requested for the worker thread; only used for the
/// reference the unload path waits on.
const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
/// Kernel priority 6: below the normal-class base 8, the mirror of Win32
/// `THREAD_PRIORITY_BELOW_NORMAL`. The merged worker spins through the
/// lock table whenever requests are absent, so it must lose every
/// scheduling contest to the game and the GUI; requests still run at once
/// whenever the CPU is free.
const WORKER_PRIORITY: i32 = 6;
/// Zero timeout: turn the request-event wait into a non-blocking poll
/// while locks are held (rewrite yields to requests, never sleeps).
const POLL_TIMEOUT: i64 = 0;
/// Liveness probe for the single-instance guard: a kernel-only marker event
/// that user-mode clients never open (they resolve only the three published
/// names), so an openable object here means another live driver instance —
/// or a concurrent second load that won the creation race — holds the claim.
const INSTANCE_MARKER_NAME: &str = "\\BaseNamedObjects\\KernelScriptInstance";
/// Registry value recording the claim under [`NAMES_KEY`]. Diagnostic
/// only: registry values outlive a crash, so the marker object above is
/// what actually decides liveness; a stale value is deleted on next start.
const INSTANCE_CLAIM_VALUE: &str = "Instance";
/// HKLM key the driver publishes its object names under; also carries the
/// single-instance claim value. Created when the claim is taken and erased
/// again by [`release_instance`], so a cleanly exited driver leaves the
/// key absent and only a crash keeps stale values behind.
const NAMES_KEY: &str = "\\Registry\\Machine\\SOFTWARE\\KernelScript";
/// Published object-name values under [`NAMES_KEY`], written by
/// [`publish_object_names`] and erased again by [`release_instance`].
const SECTION_VALUE: &str = "SectionName";
const REQUEST_EVENT_VALUE: &str = "RequestEventName";
const RESPONSE_EVENT_VALUE: &str = "ResponseEventName";

static MAPPED_BASE: AtomicUsize = AtomicUsize::new(0);
static SECTION_HANDLE: AtomicUsize = AtomicUsize::new(0);
static REQUEST_EVENT: AtomicUsize = AtomicUsize::new(0);
static RESPONSE_EVENT: AtomicUsize = AtomicUsize::new(0);
static INSTANCE_HANDLE: AtomicUsize = AtomicUsize::new(0);
static THREAD_HANDLE: AtomicUsize = AtomicUsize::new(0);
static THREAD_OBJECT: AtomicUsize = AtomicUsize::new(0);
static STOP: AtomicBool = AtomicBool::new(false);

/// Owns every kernel handle and the system-space mapping while the driver is
/// loaded. Dropping it closes them in order; [`stop`] swaps the handles out
/// of the statics first so the two paths never double close.
struct Objects {
    mapped: *mut u8,
    section: HANDLE,
    request_event: HANDLE,
    response_event: HANDLE,
}

impl Drop for Objects {
    fn drop(&mut self) {
        unsafe {
            release(
                self.mapped,
                self.section,
                self.request_event,
                self.response_event,
            )
        };
    }
}

unsafe fn release(mapped: *mut u8, section: HANDLE, request_event: HANDLE, response_event: HANDLE) {
    if !mapped.is_null() {
        let _ = MmUnmapViewInSystemSpace(mapped as *const c_void);
    }
    if !request_event.is_null() {
        let _ = ZwClose(request_event);
    }
    if !response_event.is_null() {
        let _ = ZwClose(response_event);
    }
    if !section.is_null() {
        let _ = ZwClose(section);
    }
}

/// A NUL-terminated UTF-16 object name kept alive for the `UNICODE_STRING`
/// view handed to the object manager.
struct ObjectName {
    buffer: [u16; OBJECT_NAME_CAPACITY],
    len: usize,
}

impl ObjectName {
    fn new(name: &str) -> Self {
        let mut buffer = [0u16; OBJECT_NAME_CAPACITY];
        let mut len = 0;
        for unit in name.encode_utf16() {
            if len >= OBJECT_NAME_CAPACITY - 1 {
                break;
            }
            buffer[len] = unit;
            len += 1;
        }
        Self { buffer, len }
    }

    fn as_unistring(&self) -> UNICODE_STRING {
        UNICODE_STRING {
            Length: (self.len * 2) as u16,
            MaximumLength: ((self.len + 1) * 2) as u16,
            Buffer: self.buffer.as_ptr() as *mut u16,
        }
    }

    /// A name built from a fixed `prefix` plus the shared random token —
    /// the randomized counterpart of [`ObjectName::new`] for ring objects.
    fn with_token(prefix: &str, token: &[u32; 2]) -> Self {
        let mut buffer = [0u16; OBJECT_NAME_CAPACITY];
        let mut len = 0;
        for unit in prefix.encode_utf16() {
            if len >= OBJECT_NAME_CAPACITY - 1 {
                break;
            }
            buffer[len] = unit;
            len += 1;
        }
        push_token(&mut buffer, &mut len, token);
        Self { buffer, len }
    }
}

/// Appends the token as 16 lowercase hex UTF-16 units (two `u32`
/// draws), stopping at the buffer bound (never reached: prefix + token
/// stays well under [`OBJECT_NAME_CAPACITY`]).
fn push_token(buffer: &mut [u16; OBJECT_NAME_CAPACITY], len: &mut usize, token: &[u32; 2]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for value in token {
        for shift in (0..32).step_by(4).rev() {
            let digit = ((value >> shift) & 0xF) as usize;
            if *len >= OBJECT_NAME_CAPACITY - 1 {
                return;
            }
            buffer[*len] = HEX[digit] as u16;
            *len += 1;
        }
    }
}

/// The three startup-randomized ring object names: one token shared by all
/// of them, so a client learns the complete set from the single registry
/// publication. Kept on `start`'s stack for `create_ring` and
/// `publish_object_names`; only the created handles outlive the call.
struct RingNames {
    section: ObjectName,
    request: ObjectName,
    response: ObjectName,
    token: [u32; 2],
}

impl RingNames {
    /// Fresh token for this load: interrupt time and a stack address seed
    /// the kernel PRNG, then two draws make 64 bits of name entropy. The
    /// single-instance guard ensures only one driver generates at a time.
    fn generate() -> Self {
        let mut last: u64 = 0;
        // SAFETY: out parameter is a valid local; no aliasing.
        let now = unsafe { KeQueryInterruptTimePrecise(&mut last) };
        let entropy = now ^ (now >> 32) ^ (&now as *const u64 as u64);
        // SAFETY: seed is a valid local; `RtlRandomEx` only reads and
        // rewrites it. The `| 1` keeps the classic zero-seed degeneracy out.
        let mut seed = entropy as u32 | 1;
        // SAFETY: same local seed, sequential draws.
        let token = unsafe { [RtlRandomEx(&mut seed), RtlRandomEx(&mut seed)] };
        Self {
            section: ObjectName::with_token(SECTION_NAME_PREFIX, &token),
            request: ObjectName::with_token(REQUEST_NAME_PREFIX, &token),
            response: ObjectName::with_token(RESPONSE_NAME_PREFIX, &token),
            token,
        }
    }
}

/// Absolute security descriptor granting `GENERIC_ALL` to LocalSystem
/// (`S-1-5-18`) and the Administrators group (`S-1-5-32-544`). The SIDs are
/// fixed 12/16 byte layouts, so no lookup service is needed.
///
/// The DACL pointer stored by `RtlSetDaclSecurityDescriptor` points into
/// `self.acl`, so the descriptor must be initialised in place and must not
/// move afterwards; `create_ring` keeps it in one frame for that reason.
struct SharedSecurity {
    descriptor: SECURITY_DESCRIPTOR,
    acl: [u8; DACL_CAPACITY],
    system_sid: [u8; 12],
    administrator_sid: [u8; 16],
}

impl SharedSecurity {
    fn new() -> Self {
        Self {
            // SAFETY: every field is a plain scalar/byte array.
            descriptor: unsafe { core::mem::zeroed() },
            acl: [0u8; DACL_CAPACITY],
            // S-1-5-18: rev 1, one sub-authority, 48-bit authority 5, RID 18.
            system_sid: [1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0],
            // S-1-5-32-544: two sub-authorities, RID 544 (0x220) as a
            // little-endian u32. A wrong RID here grants the DACL to a
            // group that does not exist and every client gets
            // ERROR_ACCESS_DENIED.
            administrator_sid: [1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 32, 2, 0, 0],
        }
    }

    /// # Safety
    /// Must be called on the final location of `self` and before any other
    /// field is written, because the descriptor stores a pointer into `acl`.
    unsafe fn init(&mut self) -> Result<(), NTSTATUS> {
        let mut status =
            RtlCreateSecurityDescriptor(&mut self.descriptor, SECURITY_DESCRIPTOR_REVISION);
        if !nt_success(status) {
            return Err(status);
        }
        let acl = self.acl.as_mut_ptr() as *mut ACL;
        status = RtlCreateAcl(acl, self.acl.len() as u32, ACL_REVISION);
        if !nt_success(status) {
            return Err(status);
        }
        status = RtlAddAccessAllowedAce(
            acl,
            ACL_REVISION,
            GENERIC_ALL_ACCESS,
            self.system_sid.as_mut_ptr() as PSID,
        );
        if !nt_success(status) {
            return Err(status);
        }
        status = RtlAddAccessAllowedAce(
            acl,
            ACL_REVISION,
            GENERIC_ALL_ACCESS,
            self.administrator_sid.as_mut_ptr() as PSID,
        );
        if !nt_success(status) {
            return Err(status);
        }
        status = RtlSetDaclSecurityDescriptor(
            &mut self.descriptor,
            true,
            self.acl.as_ptr() as *const ACL,
            false,
        );
        if !nt_success(status) {
            return Err(status);
        }
        Ok(())
    }

    fn descriptor(&self) -> *const SECURITY_DESCRIPTOR {
        &self.descriptor
    }
}

/// Object attributes for the ring section and events. `OBJ_KERNEL_HANDLE`
/// is mandatory: `DriverEntry` may run in an arbitrary process context —
/// the manual mapper (KDU) executes it inside the mapper's own process —
/// so handles without it would land in that process's handle table, close
/// with it the moment the mapper exits, and destroy the named ring objects
/// while the driver itself stays loaded.
fn object_attributes(name: *const UNICODE_STRING, security: &SharedSecurity) -> OBJECT_ATTRIBUTES {
    OBJECT_ATTRIBUTES {
        Length: OBJECT_ATTRIBUTES_LENGTH,
        RootDirectory: ptr::null_mut(),
        ObjectName: name,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_KERNEL_HANDLE,
        SecurityDescriptor: security.descriptor(),
        SecurityQualityOfService: ptr::null(),
    }
}

/// Open an existing named object, creating it when it does not exist yet.
/// `ZwCreateSection` reports `STATUS_OBJECT_NAME_COLLISION` instead of a
/// handle when another loader won the race, so reopen on that status too.
unsafe fn open_or_create_section(
    name: &ObjectName,
    security: &SharedSecurity,
) -> Result<HANDLE, NTSTATUS> {
    let unistring = name.as_unistring();
    let attributes = object_attributes(&unistring, security);
    let mut handle: HANDLE = ptr::null_mut();
    let mut status = ZwOpenSection(&mut handle, SECTION_ALL_ACCESS, &attributes);
    if status == STATUS_OBJECT_NAME_NOT_FOUND {
        let maximum_size = RING_TOTAL_SIZE as i64;
        status = ZwCreateSection(
            &mut handle,
            SECTION_ALL_ACCESS,
            &attributes,
            &maximum_size,
            PAGE_READWRITE,
            SEC_COMMIT,
            ptr::null_mut(),
        );
        if status == STATUS_OBJECT_NAME_COLLISION {
            handle = ptr::null_mut();
            status = ZwOpenSection(&mut handle, SECTION_ALL_ACCESS, &attributes);
        }
    }
    if !nt_success(status) {
        return Err(status);
    }
    Ok(handle)
}

unsafe fn open_or_create_event(
    name: &ObjectName,
    security: &SharedSecurity,
) -> Result<HANDLE, NTSTATUS> {
    let unistring = name.as_unistring();
    let attributes = object_attributes(&unistring, security);
    let mut handle: HANDLE = ptr::null_mut();
    // Auto-reset: the client drains the response event before every round
    // trip and the driver never leaves a request signaled.
    let mut status = ZwOpenEvent(&mut handle, EVENT_ALL_ACCESS, &attributes);
    if status == STATUS_OBJECT_NAME_NOT_FOUND {
        status = ZwCreateEvent(
            &mut handle,
            EVENT_ALL_ACCESS,
            &attributes,
            SynchronizationEvent,
            false,
        );
        if status == STATUS_OBJECT_NAME_COLLISION {
            handle = ptr::null_mut();
            status = ZwOpenEvent(&mut handle, EVENT_ALL_ACCESS, &attributes);
        }
    }
    if !nt_success(status) {
        return Err(status);
    }
    Ok(handle)
}

/// Map at least [`RING_TOTAL_SIZE`] bytes of the section into system space.
/// The section size is checked first because a section left over from an
/// older build can still be alive in the object namespace, and a too-small
/// mapping would make the ring header walk off the end.
unsafe fn map_section(section: HANDLE) -> Result<*mut u8, NTSTATUS> {
    // SAFETY: `SectionBasicInformation` is the fixed `SECTION_BASIC_INFORMATION`
    // layout the kernel expects for class 0.
    let mut info: SectionBasicInformation = core::mem::zeroed();
    let status = ZwQuerySection(
        section,
        0,
        &mut info as *mut SectionBasicInformation as *mut c_void,
        core::mem::size_of::<SectionBasicInformation>() as u32,
        ptr::null_mut(),
    );
    if !nt_success(status) {
        return Err(status);
    }
    if info.maximum_size < RING_TOTAL_SIZE as i64 {
        return Err(STATUS_INVALID_PARAMETER);
    }

    let mut object: *mut c_void = ptr::null_mut();
    let status = ObReferenceObjectByHandle(
        section,
        SECTION_ALL_ACCESS,
        0,
        KernelMode as i8,
        &mut object,
        ptr::null_mut(),
    );
    if !nt_success(status) || object.is_null() {
        return Err(if nt_success(status) {
            STATUS_INVALID_PARAMETER
        } else {
            status
        });
    }
    let mut mapped: *mut c_void = ptr::null_mut();
    let mut view_size = RING_TOTAL_SIZE;
    let status = MmMapViewInSystemSpace(object, &mut mapped, &mut view_size);
    ObDereferenceObject(object);
    if !nt_success(status) {
        return Err(status);
    }
    if mapped.is_null() {
        return Err(STATUS_INVALID_PARAMETER);
    }
    if view_size < RING_TOTAL_SIZE {
        let _ = MmUnmapViewInSystemSpace(mapped);
        return Err(STATUS_INVALID_PARAMETER);
    }
    Ok(mapped as *mut u8)
}

/// Reset the header for this load. The generation is read from the previous
/// header when it is still compatible, so a client can detect a driver reload
/// even though the section (and therefore the old generation value) survives.
unsafe fn init_header(mapped: *mut u8) {
    if let Some(head) = header(mapped) {
        let generation = if head.is_compatible() {
            head.generation().wrapping_add(1)
        } else {
            1
        };
        head.init(generation);
    }
}

unsafe fn create_ring(names: &RingNames) -> Result<Objects, NTSTATUS> {
    let mut security = SharedSecurity::new();
    security.init()?;
    let mut objects = Objects {
        mapped: ptr::null_mut(),
        section: ptr::null_mut(),
        request_event: ptr::null_mut(),
        response_event: ptr::null_mut(),
    };

    objects.section = open_or_create_section(&names.section, &security)?;
    objects.mapped = map_section(objects.section)?;
    init_header(objects.mapped);

    objects.request_event = open_or_create_event(&names.request, &security)?;
    objects.response_event = open_or_create_event(&names.response, &security)?;

    Ok(objects)
}

unsafe fn spawn_worker() -> NTSTATUS {
    let mut thread: HANDLE = ptr::null_mut();
    // Kernel handle again: the join handle must outlive whatever process
    // `DriverEntry` happened to run in (under KDU that is the mapper, which
    // exits right after the load).
    let attributes = OBJECT_ATTRIBUTES {
        Length: OBJECT_ATTRIBUTES_LENGTH,
        RootDirectory: ptr::null_mut(),
        ObjectName: ptr::null(),
        Attributes: OBJ_KERNEL_HANDLE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let status = PsCreateSystemThread(
        &mut thread,
        SYNCHRONIZE_ACCESS,
        &attributes,
        ptr::null_mut(),
        ptr::null_mut(),
        worker,
        ptr::null(),
    );
    if !nt_success(status) {
        return status;
    }
    THREAD_HANDLE.store(thread as usize, Ordering::SeqCst);

    let mut object: *mut c_void = ptr::null_mut();
    let status = ObReferenceObjectByHandle(
        thread,
        SYNCHRONIZE_ACCESS,
        0,
        KernelMode as i8,
        &mut object,
        ptr::null_mut(),
    );
    if !nt_success(status) || object.is_null() {
        let _ = ZwClose(thread);
        THREAD_HANDLE.store(0, Ordering::SeqCst);
        return if nt_success(status) {
            STATUS_INVALID_PARAMETER
        } else {
            status
        };
    }
    THREAD_OBJECT.store(object as usize, Ordering::SeqCst);
    STATUS_SUCCESS
}

/// Open the published-names key with the requested handle access.
/// `create` selects [`ZwCreateKey`] over [`ZwOpenKey`]: every path that
/// writes a value passes `true`, every delete path `false`, so cleanup can
/// never recreate the key it is about to erase.
unsafe fn open_names_key(access: u32, create: bool) -> Result<HANDLE, NTSTATUS> {
    let key_path = ObjectName::new(NAMES_KEY);
    let path = key_path.as_unistring();
    let attributes = OBJECT_ATTRIBUTES {
        Length: OBJECT_ATTRIBUTES_LENGTH,
        RootDirectory: ptr::null_mut(),
        ObjectName: &path,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_KERNEL_HANDLE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let mut key: HANDLE = ptr::null_mut();
    let mut disposition: u32 = 0;
    let status = if create {
        ZwCreateKey(
            &mut key,
            access,
            &attributes,
            0,
            ptr::null_mut(),
            0,
            &mut disposition,
        )
    } else {
        ZwOpenKey(&mut key, access, &attributes)
    };
    if !nt_success(status) {
        return Err(status);
    }
    Ok(key)
}

/// Publishes this load's randomized object names under the fixed HKLM key
/// so user-mode clients resolve them from the registry. Unlike the old
/// fixed names there is no working client fallback, so [`start`] fails the
/// load when this fails.
unsafe fn publish_object_names(names: &RingNames) -> NTSTATUS {
    let key = match open_names_key(KEY_SET_VALUE, true) {
        Ok(key) => key,
        Err(status) => return status,
    };
    let mut result = STATUS_SUCCESS;
    for (value_name, prefix) in [
        (SECTION_VALUE, SECTION_NAME_PREFIX),
        (REQUEST_EVENT_VALUE, REQUEST_NAME_PREFIX),
        (RESPONSE_EVENT_VALUE, RESPONSE_NAME_PREFIX),
    ] {
        let status = set_sz_value(key, value_name, prefix, &names.token);
        if !nt_success(status) {
            result = status;
        }
    }
    let _ = ZwClose(key);
    result
}

/// Writes one `REG_SZ` value: the object-name `prefix` plus the random
/// `token` as UTF-16, with its terminating NUL.
unsafe fn set_sz_value(key: HANDLE, value_name: &str, prefix: &str, token: &[u32; 2]) -> NTSTATUS {
    let name = ObjectName::new(value_name);
    let mut units = [0u16; OBJECT_NAME_CAPACITY];
    let mut len = 0;
    for unit in prefix.encode_utf16() {
        if len >= OBJECT_NAME_CAPACITY - 1 {
            break;
        }
        units[len] = unit;
        len += 1;
    }
    push_token(&mut units, &mut len, token);
    let value = name.as_unistring();
    ZwSetValueKey(
        key,
        &value,
        0,
        REG_SZ,
        units.as_ptr() as Pvoid,
        ((len + 1) * 2) as u32,
    )
}

/// Claim this driver load as the single instance, called before any ring
/// object exists. The registry value is only a user-mode visible record — it
/// outlives a crash and therefore cannot decide liveness; the marker object
/// does: a successful open of [`INSTANCE_MARKER_NAME`] means a live instance
/// (or a concurrent second load) holds it, and the claim is refused with
/// `STATUS_OBJECT_NAME_COLLISION`, which SCM reports as
/// `ERROR_ALREADY_EXISTS`. Kernel-mode `Zw*` calls bypass the object's DACL,
/// so the marker needs no security descriptor. A stale registry claim from
/// a crash or reboot is deleted here; the creation below (with the
/// open-on-collision retry) is the atomic gate that lets at most one
/// concurrent loader through.
unsafe fn claim_instance() -> NTSTATUS {
    let name = ObjectName::new(INSTANCE_MARKER_NAME);
    let path = name.as_unistring();
    let attributes = OBJECT_ATTRIBUTES {
        Length: OBJECT_ATTRIBUTES_LENGTH,
        RootDirectory: ptr::null_mut(),
        ObjectName: &path,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_KERNEL_HANDLE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };

    // Live instance? Only another driver ever holds this name, and this
    // driver has not created anything yet.
    let mut probe: HANDLE = ptr::null_mut();
    let status = ZwOpenEvent(&mut probe, EVENT_ALL_ACCESS, &attributes);
    if nt_success(status) {
        let _ = ZwClose(probe);
        return STATUS_OBJECT_NAME_COLLISION;
    }
    if status != STATUS_OBJECT_NAME_NOT_FOUND {
        return status;
    }

    clear_instance_claim();
    let mut handle: HANDLE = ptr::null_mut();
    let mut status = ZwCreateEvent(
        &mut handle,
        EVENT_ALL_ACCESS,
        &attributes,
        SynchronizationEvent,
        false,
    );
    if status == STATUS_OBJECT_NAME_COLLISION {
        // Lost the creation race: the winner holds the marker.
        handle = ptr::null_mut();
        status = ZwOpenEvent(&mut handle, EVENT_ALL_ACCESS, &attributes);
        if nt_success(status) {
            let _ = ZwClose(handle);
            return STATUS_OBJECT_NAME_COLLISION;
        }
        return status;
    }
    if !nt_success(status) {
        return status;
    }
    INSTANCE_HANDLE.store(handle as usize, Ordering::SeqCst);

    // Record the claim for user-mode diagnostics; the guard above already
    // holds, so a failed write never opens the gate to a second instance.
    let claim = set_instance_claim();
    if nt_success(claim) {
        crate::trace!("instance claim recorded");
    } else {
        crate::trace!("instance claim write failed 0x%lx", claim as u32);
    }
    STATUS_SUCCESS
}

/// Write the `Instance` claim value (`REG_DWORD` 1) under the names key.
unsafe fn set_instance_claim() -> NTSTATUS {
    let key = match open_names_key(KEY_SET_VALUE, true) {
        Ok(key) => key,
        Err(status) => return status,
    };
    let name = ObjectName::new(INSTANCE_CLAIM_VALUE);
    let value = name.as_unistring();
    let claim: u32 = 1;
    let status = ZwSetValueKey(
        key,
        &value,
        0,
        REG_DWORD,
        &claim as *const u32 as Pvoid,
        core::mem::size_of::<u32>() as u32,
    );
    let _ = ZwClose(key);
    status
}

/// Delete the claim value; an absent value or unreadable key is fine — the
/// value is diagnostic, and the marker object is the guard. Used when a
/// new load takes the claim (to drop a stale one); teardown uses
/// [`clear_names_registry`] instead, which removes the key entirely.
unsafe fn clear_instance_claim() {
    let Ok(key) = open_names_key(KEY_SET_VALUE | KEY_DELETE, false) else {
        return;
    };
    let name = ObjectName::new(INSTANCE_CLAIM_VALUE);
    let value = name.as_unistring();
    let _ = ZwDeleteValueKey(key, &value);
    let _ = ZwClose(key);
}

/// Erase everything this load published under [`NAMES_KEY`] — the three
/// object-name values, the `Instance` claim and the key itself — so a
/// cleanly exited driver leaves no registry state behind. Every step is
/// best-effort: a missing value or a key that cannot go away (held open by
/// a short-lived user-mode reader, which defers the deletion until that
/// handle closes) does not stop teardown, because the marker object, never
/// the registry, is what decides liveness.
unsafe fn clear_names_registry() {
    let Ok(key) = open_names_key(KEY_SET_VALUE | KEY_DELETE, false) else {
        return;
    };
    for value_name in [
        SECTION_VALUE,
        REQUEST_EVENT_VALUE,
        RESPONSE_EVENT_VALUE,
        INSTANCE_CLAIM_VALUE,
    ] {
        let name = ObjectName::new(value_name);
        let value = name.as_unistring();
        let _ = ZwDeleteValueKey(key, &value);
    }
    // The driver never creates subkeys, so the key itself can go; it must
    // carry no subkey or `ZwDeleteKey` fails, which is harmless here.
    let _ = ZwDeleteKey(key);
    let _ = ZwClose(key);
}

/// Release the single-instance claim and erase this load's registry
/// publication: the whole registry record — the three object-name values,
/// the `Instance` value and the key — goes first, the marker object last —
/// until the marker closes, a concurrent second load stays rejected while
/// teardown is still in progress.
unsafe fn release_instance() {
    clear_names_registry();
    let handle = INSTANCE_HANDLE.swap(0, Ordering::SeqCst) as HANDLE;
    if !handle.is_null() {
        let _ = ZwClose(handle);
    }
}

/// Called from `DriverEntry`. On failure every partially created object has
/// already been released before the status is returned.
pub fn start() -> NTSTATUS {
    unsafe {
        // Single instance: refuse a second load before any ring object
        // exists, so two images can never split the ring or the lock table.
        let claimed = claim_instance();
        if !nt_success(claimed) {
            crate::trace!("single-instance claim refused 0x%lx", claimed as u32);
            return claimed;
        }
        // Fresh random names for this load: one token shared by the three
        // objects, published to the registry after the ring exists.
        let names = RingNames::generate();
        let objects = match create_ring(&names) {
            Ok(objects) => objects,
            Err(status) => {
                // Releases the claim taken above; with no objects created
                // yet, everything else in `stop` is a no-op.
                stop();
                return status;
            }
        };
        MAPPED_BASE.store(objects.mapped as usize, Ordering::SeqCst);
        SECTION_HANDLE.store(objects.section as usize, Ordering::SeqCst);
        REQUEST_EVENT.store(objects.request_event as usize, Ordering::SeqCst);
        RESPONSE_EVENT.store(objects.response_event as usize, Ordering::SeqCst);
        core::mem::forget(objects);

        crate::trace!(
            "ring ready mapped=%p",
            MAPPED_BASE.load(Ordering::SeqCst) as *mut u8
        );
        // The lock table's fast mutex must exist before the first `Lock`
        // request can arrive; initialisation cannot fail.
        crate::lock::init();
        let status = spawn_worker();
        if !nt_success(status) {
            crate::trace!("worker spawn failed 0x%lx", status as u32);
            stop();
            return status;
        }
        crate::trace!("worker spawned");

        // With randomized names the registry publication is the only way a
        // client can learn them — no fallback exists — so a failed write
        // fails the whole load instead of leaving an unreachable driver.
        let published = publish_object_names(&names);
        if nt_success(published) {
            crate::trace!("object names published to registry");
        } else {
            crate::trace!("registry publish failed 0x%lx", published as u32);
            stop();
            return published;
        }
        STATUS_SUCCESS
    }
}

/// Releases everything [`start`] acquired, from the worker itself when a
/// `Shutdown` request arrives. Every handle is swapped out of its static
/// first, so this and [`stop`] — whichever runs first — owns each handle
/// and the other observes NULL: no handle is ever closed twice.
///
/// `THREAD_OBJECT` is deliberately left untouched: [`stop`] must still be
/// able to join through it after a `Shutdown` + `sc stop` sequence. A
/// manually mapped image never unloads, so its thread object and the
/// worker's kernel stack are never collected — an accepted leak of one
/// thread per mapped instance.
///
/// # Safety
/// Runs on the worker thread itself with no concurrent sweep (this *is*
/// the worker); the ring handles it swaps are only used by this thread
/// from here on, and the client still holds its own references to the
/// shared objects.
unsafe fn self_teardown() {
    let thread_handle = THREAD_HANDLE.swap(0, Ordering::SeqCst) as HANDLE;
    if !thread_handle.is_null() {
        let _ = ZwClose(thread_handle);
    }
    let request_event = REQUEST_EVENT.swap(0, Ordering::SeqCst) as HANDLE;
    let response_event = RESPONSE_EVENT.swap(0, Ordering::SeqCst) as HANDLE;
    let section = SECTION_HANDLE.swap(0, Ordering::SeqCst) as HANDLE;
    let mapped = MAPPED_BASE.swap(0, Ordering::SeqCst) as *mut u8;
    release(mapped, section, request_event, response_event);
    // Last: the marker object must outlive the ring teardown so a racing
    // second load stays rejected until this instance is fully gone.
    release_instance();
}

/// Called from `DriverUnload`. Idempotent: every handle is swapped out of
/// its static before it is closed, so a second call — or a [`self_teardown`]
/// that already ran — is a no-op.
///
/// # Safety
/// No client may be inside [`worker`] when this runs; the wait on the thread
/// object guarantees that.
pub unsafe fn stop() {
    STOP.store(true, Ordering::SeqCst);

    let request_event = REQUEST_EVENT.swap(0, Ordering::SeqCst) as HANDLE;
    if !request_event.is_null() {
        let _ = ZwSetEvent(request_event, ptr::null_mut());
    }

    let thread_object = THREAD_OBJECT.swap(0, Ordering::SeqCst);
    if thread_object != 0 {
        let object = thread_object as Pvoid;
        let _ = KeWaitForSingleObject(
            object as *const c_void,
            Executive,
            KernelMode as i8,
            false,
            ptr::null(),
        );
        ObDereferenceObject(object);
    }

    let thread_handle = THREAD_HANDLE.swap(0, Ordering::SeqCst) as HANDLE;
    if !thread_handle.is_null() {
        let _ = ZwClose(thread_handle);
    }
    // The ring worker is joined, so no sweep can race the final table
    // clear; dropping the table keeps every target write inside the
    // driver's lifetime.
    crate::lock::clear_table();
    let response_event = RESPONSE_EVENT.swap(0, Ordering::SeqCst) as HANDLE;
    let section = SECTION_HANDLE.swap(0, Ordering::SeqCst) as HANDLE;
    let mapped = MAPPED_BASE.swap(0, Ordering::SeqCst) as *mut u8;
    release(mapped, section, request_event, response_event);
    // Last: the marker object must outlive the ring teardown so a racing
    // second load stays rejected until this instance is fully gone.
    release_instance();
    STOP.store(false, Ordering::SeqCst);
}

unsafe extern "system" fn worker(_context: *mut c_void) {
    crate::trace!("worker enter");
    let _ = KeSetPriorityThread(PsGetCurrentThread(), WORKER_PRIORITY);
    let base = MAPPED_BASE.load(Ordering::SeqCst) as *mut u8;
    let request_event = REQUEST_EVENT.load(Ordering::SeqCst) as HANDLE;
    let response_event = RESPONSE_EVENT.load(Ordering::SeqCst) as HANDLE;
    loop {
        // Merged request/lock loop: with locks held the wait is a pure
        // poll (timeout 0), so a pending request always wins over the
        // next rewrite entry; with the table empty the wait blocks, so an
        // idle driver burns no CPU — the `Lock` request itself is what
        // wakes it, and no private wake event exists.
        let timeout: *const i64 = if crate::lock::has_entries() {
            &POLL_TIMEOUT
        } else {
            ptr::null()
        };
        let status = ZwWaitForSingleObject(request_event, false, timeout);
        if !nt_success(status) {
            crate::trace!("wait failed 0x%lx", status as u32);
            break;
        }
        if STOP.load(Ordering::SeqCst) {
            break;
        }
        let Some(head) = header(base) else {
            break;
        };
        // The client only publishes REQUEST after it has finished writing
        // the payload, and only the driver may move it to PROCESSING, so a
        // spurious wakeup just loops back to the wait.
        if head.state() == STATE_REQUEST && head.cas_state(STATE_REQUEST, STATE_PROCESSING) {
            crate::trace!(
                "request seq=%llu len=%lu",
                head.sequence(),
                head.request_len()
            );
            let request = request_bytes(base);
            let response = response_bytes(base);
            let keep_serving = request::process_request(head, request, response);
            crate::trace!(
                "respond st=0x%lx seq=%llu",
                head.status() as u32,
                head.response_sequence()
            );
            let _ = ZwSetEvent(response_event, ptr::null_mut());
            if !keep_serving {
                crate::trace!("shutdown requested - worker exiting");
                // The response is already published; drop the lock table
                // so a shut-down driver performs no further target writes.
                crate::lock::clear_table();
                // Then release everything `start` acquired. A manually
                // mapped image (KDU) never runs `DriverUnload`, so this is
                // its only chance to free the ring objects, the registry
                // claim and the single-instance marker; under the SCM load
                // path `stop` repeats the same swaps after joining this
                // thread and finds them already NULL.
                self_teardown();
                break;
            }
            // Queued requests are served before any further rewriting.
            continue;
        }
        // No request pending: rewrite at most one lock entry per pass,
        // polling the event again immediately afterwards. A table that
        // became empty just loops back to the blocking wait.
        let _ = crate::lock::sweep_step();
    }
    crate::trace!("worker exit");
    let _ = PsTerminateSystemThread(STATUS_SUCCESS);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid(sid: &[u8]) -> u32 {
        u32::from_le_bytes([
            sid[sid.len() - 4],
            sid[sid.len() - 3],
            sid[sid.len() - 2],
            sid[sid.len() - 1],
        ])
    }

    #[test]
    fn security_descriptor_sids_target_system_and_administrators() {
        let security = SharedSecurity::new();
        assert_eq!(security.system_sid[0], 1);
        assert_eq!(security.system_sid[1], 1);
        assert_eq!(rid(&security.system_sid), 18, "LocalSystem RID");

        assert_eq!(security.administrator_sid[0], 1);
        assert_eq!(security.administrator_sid[1], 2);
        // BUILTIN\Administrators is 544 = 0x220; 744 or any other RID grants
        // the DACL to a group that does not exist.
        assert_eq!(rid(&security.administrator_sid), 544, "Administrators RID");
        assert_eq!(
            &security.administrator_sid[2..8],
            &[0, 0, 0, 0, 0, 5],
            "NT authority"
        );
        assert_eq!(
            &security.administrator_sid[8..12],
            &32u32.to_le_bytes(),
            "BUILTIN domain"
        );
    }
}
