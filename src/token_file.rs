use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::path::Path;

#[cfg(unix)]
pub(crate) fn create_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating bearer token file {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("writing bearer token file {}", path.display()))
}

#[cfg(unix)]
fn validate_unix_metadata(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        anyhow::bail!(
            "bearer token file {} must not be accessible by group or other users; run chmod 600 {}",
            path.display(),
            path.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn read_private(path: &Path) -> Result<Option<String>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting token file {}", path.display()))
        }
    };
    if metadata.file_type().is_symlink() {
        anyhow::bail!("bearer token file {} must not be a symlink", path.display());
    }
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening token file {}", path.display()))?;
    let opened_metadata = file
        .metadata()
        .with_context(|| format!("inspecting opened token file {}", path.display()))?;
    validate_unix_metadata(path, &opened_metadata)?;
    let mut value = String::new();
    file.read_to_string(&mut value)
        .with_context(|| format!("reading token file {}", path.display()))?;
    Ok(Some(value))
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(windows)]
struct LocalDescriptor(windows_sys::Win32::Security::PSECURITY_DESCRIPTOR);

#[cfg(windows)]
impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(self.0 as _);
            }
        }
    }
}

#[cfg(windows)]
struct LocalWideString(windows_sys::core::PWSTR);

#[cfg(windows)]
impl Drop for LocalWideString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(self.0 as _);
            }
        }
    }
}

#[cfg(windows)]
pub(crate) fn create_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::{
        Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::{CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL},
    };

    let current_user_sid = current_user_sid_string()?;
    let descriptor_text: Vec<u16> =
        format!("O:{current_user_sid}D:P(A;;FA;;;OW)(A;;FA;;;SY)(A;;FA;;;BA)")
            .encode_utf16()
            .chain(Some(0))
            .collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor_text.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(std::io::Error::last_os_error())
            .context("building the private Windows token security descriptor");
    }
    let descriptor = LocalDescriptor(descriptor);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let path_wide = wide_path(path);
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            GENERIC_WRITE,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("creating bearer token file {}", path.display()));
    }
    let mut file = unsafe { std::fs::File::from_raw_handle(handle as _) };
    file.write_all(contents)
        .with_context(|| format!("writing bearer token file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("flushing bearer token file {}", path.display()))?;
    drop(file);
    validate_private(path)
}

#[cfg(windows)]
fn well_known_sid(kind: windows_sys::Win32::Security::WELL_KNOWN_SID_TYPE) -> Result<Vec<u8>> {
    use windows_sys::Win32::Security::{CreateWellKnownSid, SECURITY_MAX_SID_SIZE};

    let mut sid = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut size = sid.len() as u32;
    let created =
        unsafe { CreateWellKnownSid(kind, std::ptr::null_mut(), sid.as_mut_ptr() as _, &mut size) };
    if created == 0 {
        return Err(std::io::Error::last_os_error()).context("creating a well-known Windows SID");
    }
    sid.truncate(size as usize);
    Ok(sid)
}

#[cfg(windows)]
fn current_user_token_info() -> Result<Vec<usize>> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{GetTokenInformation, TokenUser, TOKEN_QUERY},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    struct HandleGuard(HANDLE);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error()).context("opening the current Windows token");
    }
    let token = HandleGuard(token);
    let mut required = 0u32;
    unsafe {
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut required);
    }
    if required == 0 {
        return Err(std::io::Error::last_os_error()).context("sizing the current Windows user SID");
    }
    let word = std::mem::size_of::<usize>();
    let mut buffer = vec![0usize; (required as usize).div_ceil(word)];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr() as _,
            required,
            &mut required,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .context("reading the current Windows user SID");
    }
    Ok(buffer)
}

#[cfg(windows)]
fn current_user_sid_string() -> Result<String> {
    use windows_sys::Win32::Security::{Authorization::ConvertSidToStringSidW, TOKEN_USER};

    let buffer = current_user_token_info()?;
    let current_user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
    let mut sid_text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(current_user.User.Sid, &mut sid_text) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("formatting the current Windows user SID");
    }
    let sid_text = LocalWideString(sid_text);
    let mut length = 0;
    while unsafe { *sid_text.0.add(length) } != 0 {
        length += 1;
    }
    String::from_utf16(unsafe { std::slice::from_raw_parts(sid_text.0, length) })
        .context("decoding the current Windows user SID")
}

#[cfg(windows)]
pub(crate) fn validate_private(path: &Path) -> Result<()> {
    use windows_sys::Win32::Security::{
        Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
        ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };

    let path_wide = wide_path(path);
    let mut owner: PSID = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        anyhow::bail!(
            "cannot inspect Windows ACL for bearer token {}: OS error {}",
            path.display(),
            status
        );
    }
    validate_windows_descriptor(path, owner, dacl, LocalDescriptor(descriptor))
}

#[cfg(windows)]
fn validate_windows_descriptor(
    path: &Path,
    owner: windows_sys::Win32::Security::PSID,
    dacl: *mut windows_sys::Win32::Security::ACL,
    descriptor_guard: LocalDescriptor,
) -> Result<()> {
    use windows_sys::Win32::{
        Security::{
            EqualSid, GetAce, GetSecurityDescriptorControl, WinBuiltinAdministratorsSid,
            WinCreatorOwnerRightsSid, WinLocalSystemSid, ACCESS_ALLOWED_ACE, PSID,
            SE_DACL_PROTECTED,
        },
        System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE},
    };

    let mut control = 0u16;
    let mut revision = 0u32;
    if unsafe { GetSecurityDescriptorControl(descriptor_guard.0, &mut control, &mut revision) } == 0
    {
        return Err(std::io::Error::last_os_error()).context("reading Windows token DACL control");
    }
    if control & SE_DACL_PROTECTED == 0 || dacl.is_null() || owner.is_null() {
        anyhow::bail!(
            "bearer token file {} must have a protected, explicit Windows DACL",
            path.display()
        );
    }

    let current_user_buffer = current_user_token_info()?;
    let current_user = unsafe {
        &*(current_user_buffer.as_ptr() as *const windows_sys::Win32::Security::TOKEN_USER)
    };
    if unsafe { EqualSid(owner, current_user.User.Sid) } == 0 {
        anyhow::bail!(
            "bearer token file {} is not owned by the current Windows user",
            path.display()
        );
    }

    let system = well_known_sid(WinLocalSystemSid)?;
    let admins = well_known_sid(WinBuiltinAdministratorsSid)?;
    let owner_rights = well_known_sid(WinCreatorOwnerRightsSid)?;
    let allowed = [
        owner,
        system.as_ptr() as PSID,
        admins.as_ptr() as PSID,
        owner_rights.as_ptr() as PSID,
    ];
    let ace_count = unsafe { (*dacl).AceCount as u32 };
    for index in 0..ace_count {
        let mut raw_ace = std::ptr::null_mut();
        if unsafe { GetAce(dacl, index, &mut raw_ace) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("reading Windows token DACL entry");
        }
        let header = unsafe { &*(raw_ace as *const windows_sys::Win32::Security::ACE_HEADER) };
        if header.AceType as u32 == ACCESS_DENIED_ACE_TYPE {
            continue;
        }
        if header.AceType as u32 != ACCESS_ALLOWED_ACE_TYPE {
            anyhow::bail!(
                "bearer token file {} contains an unsupported Windows access entry",
                path.display()
            );
        }
        let ace = unsafe { &*(raw_ace as *const ACCESS_ALLOWED_ACE) };
        let sid = &ace.SidStart as *const u32 as PSID;
        let trusted = allowed
            .iter()
            .any(|candidate| unsafe { EqualSid(sid, *candidate) } != 0);
        if !trusted {
            anyhow::bail!(
                "bearer token file {} grants access to an unrelated Windows principal",
                path.display()
            );
        }
    }

    Ok(())
}

#[cfg(windows)]
fn validate_private_handle(
    path: &Path,
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> Result<()> {
    use windows_sys::Win32::Security::{
        Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
        ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };

    let mut owner: PSID = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        anyhow::bail!(
            "cannot inspect Windows ACL for bearer token {}: OS error {}",
            path.display(),
            status
        );
    }
    validate_windows_descriptor(path, owner, dacl, LocalDescriptor(descriptor))
}

#[cfg(windows)]
pub(crate) fn read_private(path: &Path) -> Result<Option<String>> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            CreateFileW, FileAttributeTagInfo, GetFileInformationByHandleEx,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_OPEN_REPARSE_POINT,
            OPEN_EXISTING, READ_CONTROL,
        },
    };

    let path_wide = wide_path(path);
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            GENERIC_READ | READ_CONTROL,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("opening token file {}", path.display()));
    }
    let mut file = unsafe { std::fs::File::from_raw_handle(handle as _) };
    let mut tag_info = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    let inspected = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            &mut tag_info as *mut _ as _,
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if inspected == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("inspecting token file {}", path.display()));
    }
    if tag_info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        anyhow::bail!(
            "bearer token file {} must not be a reparse point",
            path.display()
        );
    }
    validate_private_handle(path, handle)?;
    let mut value = String::new();
    file.read_to_string(&mut value)
        .with_context(|| format!("reading token file {}", path.display()))?;
    Ok(Some(value))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn create_private(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating bearer token file {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("writing bearer token file {}", path.display()))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn validate_private(_path: &Path) -> Result<()> {
    anyhow::bail!("this platform does not provide a supported private token-file permission check")
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn read_private(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(_) => validate_private(path).map(|()| None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading token file {}", path.display())),
    }
}
