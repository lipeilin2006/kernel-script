//! Ring object creation: section open/create, system-space mapping, header
//! reset and the event pair — plus the handle-release helper every teardown
//! path shares.

use core::ffi::c_void;
use core::ptr;

use ks_core::ring::{header, RING_TOTAL_SIZE};

use super::names::{ObjectName, RingNames};
use super::security::{object_attributes, SharedSecurity};
use crate::wdm::*;

/// Owns every kernel handle and the system-space mapping while the driver is
/// loaded. Dropping it closes them in order; [`super::stop`] swaps the
/// handles out of the statics first so the two paths never double close.
pub(super) struct Objects {
    pub(super) mapped: *mut u8,
    pub(super) section: HANDLE,
    pub(super) request_event: HANDLE,
    pub(super) response_event: HANDLE,
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

pub(super) unsafe fn release(
    mapped: *mut u8,
    section: HANDLE,
    request_event: HANDLE,
    response_event: HANDLE,
) {
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

pub(super) unsafe fn create_ring(names: &RingNames) -> Result<Objects, NTSTATUS> {
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
