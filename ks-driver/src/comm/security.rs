//! The shared section DACL (SYSTEM + Administrators, `GENERIC_ALL`) and
//! the `OBJECT_ATTRIBUTES` builder that hands it to the object manager.

use core::ptr;

use crate::wdm::*;

/// Section DACL: `SECURITY_DESCRIPTOR` header plus room for two ACEs.
const DACL_CAPACITY: usize = 128;

/// Absolute security descriptor granting `GENERIC_ALL` to LocalSystem
/// (`S-1-5-18`) and the Administrators group (`S-1-5-32-544`). The SIDs are
/// fixed 12/16 byte layouts, so no lookup service is needed.
///
/// The DACL pointer stored by `RtlSetDaclSecurityDescriptor` points into
/// `self.acl`, so the descriptor must be initialised in place and must not
/// move afterwards; `create_ring` keeps it in one frame for that reason.
pub(super) struct SharedSecurity {
    descriptor: SECURITY_DESCRIPTOR,
    acl: [u8; DACL_CAPACITY],
    system_sid: [u8; 12],
    administrator_sid: [u8; 16],
}

impl SharedSecurity {
    pub(super) fn new() -> Self {
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
    pub(super) unsafe fn init(&mut self) -> Result<(), NTSTATUS> {
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

    pub(super) fn descriptor(&self) -> *const SECURITY_DESCRIPTOR {
        &self.descriptor
    }
}

/// Object attributes for the ring section and events. `OBJ_KERNEL_HANDLE`
/// is mandatory: `DriverEntry` may run in an arbitrary process context —
/// the manual mapper (KDU) executes it inside the mapper's own process —
/// so handles without it would land in that process's handle table, close
/// with it the moment the mapper exits, and destroy the named ring objects
/// while the driver itself stays loaded.
pub(super) fn object_attributes(
    name: *const UNICODE_STRING,
    security: &SharedSecurity,
) -> OBJECT_ATTRIBUTES {
    OBJECT_ATTRIBUTES {
        Length: OBJECT_ATTRIBUTES_LENGTH,
        RootDirectory: ptr::null_mut(),
        ObjectName: name,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_KERNEL_HANDLE,
        SecurityDescriptor: security.descriptor(),
        SecurityQualityOfService: ptr::null(),
    }
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
