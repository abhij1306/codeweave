//! Win32 access control for the bearer token file.
//!
//! This is the only `unsafe` code of the token-file module. Each block names
//! the invariant it relies on. Memory and handles the system hands back are
//! owned by RAII guards ([`LocalMemory`], [`OwnedHandle`]), and the functions
//! the parent module calls are safe.
use super::TokenFileError;
use std::ffi::{c_void, OsStr};
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::ptr::{null_mut, NonNull};
use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    CreateWellKnownSid, EqualSid, GetAce, GetSecurityDescriptorControl, GetTokenInformation,
    TokenUser, WinBuiltinAdministratorsSid, WinCreatorOwnerRightsSid, WinLocalSystemSid,
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, SE_DACL_PROTECTED,
    TOKEN_QUERY, TOKEN_USER, WELL_KNOWN_SID_TYPE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_OPEN_REPARSE_POINT, READ_CONTROL,
};
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Create `path` with an owner-only protected DACL, write `contents`, and
/// confirm the DACL the file ended up with.
pub(super) fn create_private(path: &Path, contents: &[u8]) -> Result<(), TokenFileError> {
    let user = CurrentUser::query()
        .and_then(|user| user.sid_string())
        .map_err(|error| TokenFileError::io("reading the current Windows user for", path, error))?;
    let descriptor = LocalMemory::security_descriptor(&private_sddl(&user))
        .map_err(|error| TokenFileError::io("building the security descriptor for", path, error))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let path_wide = wide(path.as_os_str());
    // SAFETY: `path_wide` is NUL-terminated and `attributes` (with the
    // descriptor it points to) outlives the call. `CREATE_NEW` fails rather
    // than open an existing file, so the returned handle is a new file.
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            GENERIC_WRITE | READ_CONTROL,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(TokenFileError::io(
            "creating",
            path,
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: `handle` is a valid file handle that nothing else owns; the
    // `File` closes it.
    let mut file = unsafe { File::from_raw_handle(handle as _) };
    file.write_all(contents)
        .map_err(|error| TokenFileError::io("writing", path, error))?;
    file.sync_all()
        .map_err(|error| TokenFileError::io("flushing", path, error))?;
    ensure_private(path, &file)
}

/// Open `path` without following a reparse point, and only when its owner is
/// the current user and its DACL admits nobody else.
pub(super) fn open_private(path: &Path) -> Result<Option<File>, TokenFileError> {
    let file = match OpenOptions::new()
        .access_mode(GENERIC_READ | READ_CONTROL)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(TokenFileError::io("opening", path, error)),
    };
    let attributes = file
        .metadata()
        .map_err(|error| TokenFileError::io("inspecting", path, error))?
        .file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(TokenFileError::Link {
            path: path.to_path_buf(),
        });
    }
    ensure_private(path, &file)?;
    Ok(Some(file))
}

/// The descriptor of a new token file: owned by `user_sid`, protected from
/// inheritance, full access for OWNER RIGHTS, SYSTEM and Administrators.
fn private_sddl(user_sid: &str) -> String {
    format!("O:{user_sid}D:P(A;;FA;;;OW)(A;;FA;;;SY)(A;;FA;;;BA)")
}

fn ensure_private(path: &Path, file: &File) -> Result<(), TokenFileError> {
    let info = SecurityInfo::of_file(file)
        .map_err(|error| TokenFileError::io("reading the Windows ACL of", path, error))?;
    match info
        .violation()
        .map_err(|error| TokenFileError::io("checking the Windows ACL of", path, error))?
    {
        Some(reason) => Err(TokenFileError::not_private(path, reason)),
        None => Ok(()),
    }
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

/// Memory a Win32 call allocated with `LocalAlloc`, freed once on drop.
struct LocalMemory(*mut c_void);

impl LocalMemory {
    /// A self-relative security descriptor parsed from `sddl`.
    fn security_descriptor(sddl: &str) -> io::Result<Self> {
        let text = wide(OsStr::new(sddl));
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `text` is NUL-terminated and `descriptor` is a valid
        // out-pointer; the size out-parameter is optional.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }
}

impl Drop for LocalMemory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from a Win32 call documented to
            // allocate with `LocalAlloc`, and this guard is its only owner.
            unsafe {
                LocalFree(self.0 as _);
            }
        }
    }
}

/// A kernel handle closed once on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened for this guard, which is its only
        // owner.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// The current process user: a `TOKEN_USER` in a pointer-aligned buffer.
struct CurrentUser(Vec<usize>);

impl CurrentUser {
    fn query() -> io::Result<Self> {
        let mut token: HANDLE = null_mut();
        // SAFETY: `token` is a valid out-pointer; the process pseudo-handle
        // needs no closing.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle(token);
        let mut required = 0u32;
        // SAFETY: a null buffer of length 0 only asks for the required size.
        unsafe {
            GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut required);
        }
        if required == 0 {
            return Err(io::Error::last_os_error());
        }
        let word = std::mem::size_of::<usize>();
        let mut buffer = vec![0usize; (required as usize).div_ceil(word)];
        // SAFETY: `buffer` has at least `required` writable bytes.
        let read = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr() as _,
                required,
                &mut required,
            )
        };
        if read == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(buffer))
    }

    /// The user's SID, valid while `self` lives.
    fn sid(&self) -> PSID {
        // SAFETY: `query` filled the buffer with a `TOKEN_USER`, which a
        // `usize` buffer is aligned for, and its SID pointer targets memory
        // inside the same buffer.
        unsafe { (*(self.0.as_ptr() as *const TOKEN_USER)).User.Sid }
    }

    fn sid_string(&self) -> io::Result<String> {
        sid_string(self.sid())
    }
}

/// The `S-1-...` text of `sid`.
fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text: *mut u16 = null_mut();
    // SAFETY: `sid` is valid for the call and `text` is a valid out-pointer.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = LocalMemory(text as _);
    let mut length = 0;
    // SAFETY: the system returned a NUL-terminated UTF-16 string, which
    // `owned` keeps alive; the scan stops at the terminator.
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: the first `length` units were just read as initialized.
    let units = unsafe { std::slice::from_raw_parts(text, length) };
    let value =
        String::from_utf16(units).map_err(|error| io::Error::new(ErrorKind::InvalidData, error));
    drop(owned);
    value
}

/// A well-known principal's SID.
struct WellKnownSid(Vec<u8>);

impl WellKnownSid {
    fn new(kind: WELL_KNOWN_SID_TYPE) -> io::Result<Self> {
        let mut sid = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut size = sid.len() as u32;
        // SAFETY: `sid` has `size` writable bytes, the largest a SID can be.
        let created =
            unsafe { CreateWellKnownSid(kind, null_mut(), sid.as_mut_ptr() as _, &mut size) };
        if created == 0 {
            return Err(io::Error::last_os_error());
        }
        sid.truncate(size as usize);
        Ok(Self(sid))
    }

    fn as_psid(&self) -> PSID {
        self.0.as_ptr() as PSID
    }
}

fn equal_sid(left: PSID, right: PSID) -> bool {
    // SAFETY: callers pass SIDs that stay valid for the call.
    unsafe { EqualSid(left, right) != 0 }
}

/// One DACL entry.
enum Entry {
    Allowed(PSID),
    Denied,
    Unsupported,
}

/// A file's owner and DACL. Both point into `descriptor`, so they are valid
/// while `self` lives. `dacl` is `None` when the file has no DACL.
struct SecurityInfo {
    owner: PSID,
    dacl: Option<NonNull<ACL>>,
    descriptor: LocalMemory,
}

impl SecurityInfo {
    fn of_file(file: &File) -> io::Result<Self> {
        let mut owner: PSID = null_mut();
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: the handle is open with `READ_CONTROL` for the call and
        // every out-pointer is valid; group and SACL are not requested.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        let descriptor = LocalMemory(descriptor);
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(Self {
            owner,
            dacl: NonNull::new(dacl),
            descriptor,
        })
    }

    fn protected(&self) -> io::Result<bool> {
        let mut control = 0u16;
        let mut revision = 0u32;
        // SAFETY: `descriptor` is the valid descriptor `GetSecurityInfo`
        // returned and the out-pointers are valid.
        let read =
            unsafe { GetSecurityDescriptorControl(self.descriptor.0, &mut control, &mut revision) };
        if read == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(control & SE_DACL_PROTECTED != 0)
    }

    /// The DACL's entries in order; empty when there is no DACL.
    fn entries(&self) -> io::Result<Vec<Entry>> {
        let Some(dacl) = self.dacl else {
            return Ok(Vec::new());
        };
        // SAFETY: `dacl` is non-null and points to a valid ACL inside
        // `descriptor`, which `self` keeps alive.
        let count = u32::from(unsafe { dacl.as_ref() }.AceCount);
        let mut entries = Vec::new();
        for index in 0..count {
            let mut raw: *mut c_void = null_mut();
            // SAFETY: `index` is below the ACL's entry count and `raw` is a
            // valid out-pointer.
            if unsafe { GetAce(dacl.as_ptr(), index, &mut raw) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let Some(raw) = NonNull::new(raw) else {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "GetAce returned a null entry",
                ));
            };
            // SAFETY: `raw` is a non-null ACE inside the ACL, and every ACE
            // starts with an `ACE_HEADER`.
            let kind = u32::from(unsafe { raw.cast::<ACE_HEADER>().as_ref() }.AceType);
            entries.push(if kind == ACCESS_ALLOWED_ACE_TYPE {
                // SAFETY: the header says this is an `ACCESS_ALLOWED_ACE`,
                // whose SID starts at `SidStart`.
                let ace = unsafe { raw.cast::<ACCESS_ALLOWED_ACE>().as_ref() };
                Entry::Allowed(&ace.SidStart as *const u32 as PSID)
            } else if kind == ACCESS_DENIED_ACE_TYPE {
                Entry::Denied
            } else {
                Entry::Unsupported
            });
        }
        Ok(entries)
    }

    /// Why the file is not private to the current user, if it is not.
    fn violation(&self) -> io::Result<Option<&'static str>> {
        if !self.protected()? || self.dacl.is_none() || self.owner.is_null() {
            return Ok(Some("must have a protected, explicit Windows DACL"));
        }
        let user = CurrentUser::query()?;
        if !equal_sid(self.owner, user.sid()) {
            return Ok(Some("is not owned by the current Windows user"));
        }
        let trusted = [
            WellKnownSid::new(WinLocalSystemSid)?,
            WellKnownSid::new(WinBuiltinAdministratorsSid)?,
            WellKnownSid::new(WinCreatorOwnerRightsSid)?,
        ];
        for entry in self.entries()? {
            match entry {
                Entry::Denied => {}
                Entry::Unsupported => {
                    return Ok(Some("contains an unsupported Windows access entry"));
                }
                Entry::Allowed(sid) => {
                    let known = equal_sid(sid, self.owner)
                        || trusted
                            .iter()
                            .any(|candidate| equal_sid(sid, candidate.as_psid()));
                    if !known {
                        return Ok(Some("grants access to an unrelated Windows principal"));
                    }
                }
            }
        }
        Ok(None)
    }
}

/// What a test can observe about a token file's access control.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Access {
    pub(super) owned_by_current_user: bool,
    pub(super) protected: bool,
    /// SID text of each access-allowed entry, in DACL order.
    pub(super) allowed: Vec<String>,
}

#[cfg(test)]
pub(super) fn access(path: &Path) -> io::Result<Access> {
    let file = OpenOptions::new().access_mode(READ_CONTROL).open(path)?;
    let info = SecurityInfo::of_file(&file)?;
    let user = CurrentUser::query()?;
    let allowed = info
        .entries()?
        .into_iter()
        .filter_map(|entry| match entry {
            Entry::Allowed(sid) => Some(sid_string(sid)),
            Entry::Denied | Entry::Unsupported => None,
        })
        .collect::<io::Result<Vec<_>>>()?;
    Ok(Access {
        owned_by_current_user: !info.owner.is_null() && equal_sid(info.owner, user.sid()),
        protected: info.protected()?,
        allowed,
    })
}
