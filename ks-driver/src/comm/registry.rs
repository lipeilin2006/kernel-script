//! Registry publication and the single-instance marker claim: the key
//! `HKLM\SOFTWARE\KernelScript` is the client's only discovery path for the
//! randomized names, and the marker event is the liveness probe both loads
//! must agree on.

use core::ptr;
use core::sync::atomic::Ordering;

use crate::wdm::*;

use super::names::{push_token, ObjectName};
use super::{
    INSTANCE_HANDLE, OBJECT_NAME_CAPACITY, REQUEST_NAME_PREFIX, RESPONSE_NAME_PREFIX,
    SECTION_NAME_PREFIX,
};

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
/// fixed names there is no working client fallback, so [`super::start`] fails
/// the load when this fails.
pub(super) unsafe fn publish_object_names(names: &super::names::RingNames) -> NTSTATUS {
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
pub(super) unsafe fn claim_instance() -> NTSTATUS {
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
pub(super) unsafe fn release_instance() {
    clear_names_registry();
    let handle = INSTANCE_HANDLE.swap(0, Ordering::SeqCst) as HANDLE;
    if !handle.is_null() {
        let _ = ZwClose(handle);
    }
}
