//! Current-user ownership for private Windows objects.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, DACL_SECURITY_INFORMATION, GetAce, GetTokenInformation,
    OWNER_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, GetFileInformationByHandle, READ_CONTROL,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

pub(crate) struct Handle(pub HANDLE);

// Kernel handles have no thread affinity and are closed by their sole owner.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    pub(crate) fn new(raw: HANDLE) -> io::Result<Self> {
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

pub(crate) struct LocalAllocation(pub *mut std::ffi::c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

pub(crate) fn wide(value: impl AsRef<std::ffi::OsStr>) -> io::Result<Vec<u16>> {
    let mut value: Vec<u16> = value.as_ref().encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "embedded NUL in Windows object name",
        ));
    }
    value.push(0);
    Ok(value)
}

fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _allocation = LocalAllocation(text.cast());
    let mut len = 0;
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    Ok(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(text, len)
    }))
}

fn process_sid(process: HANDLE) -> io::Result<String> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Handle::new(token)?;
    let mut size = 0;
    unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut size) };
    if size == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    sid_string(user.User.Sid)
}

pub(crate) fn current_sid() -> io::Result<String> {
    process_sid(unsafe { GetCurrentProcess() })
}

pub(crate) fn require_process_user(pid: u32, expected: &str) -> io::Result<()> {
    let process = Handle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })?;
    if process_sid(process.0)? != expected {
        return Err(denied("the local RC peer belongs to another Windows user"));
    }
    Ok(())
}

pub(crate) fn private_descriptor(sid: &str, directory: bool) -> io::Result<LocalAllocation> {
    let inherit = if directory { "OICI" } else { "" };
    let sddl = wide(format!("O:{sid}D:P(A;{inherit};GA;;;{sid})"))?;
    let mut descriptor = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalAllocation(descriptor))
}

pub(crate) fn attributes(descriptor: &LocalAllocation) -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    }
}

pub(crate) fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

pub(crate) fn directory_identity(path: &Path) -> io::Result<[u8; 12]> {
    validate_path(path, true, true)?;
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut key = [0; 12];
    key[..4].copy_from_slice(&info.dwVolumeSerialNumber.to_le_bytes());
    key[4..8].copy_from_slice(&info.nFileIndexHigh.to_le_bytes());
    key[8..].copy_from_slice(&info.nFileIndexLow.to_le_bytes());
    Ok(key)
}

/// Existing objects are inspected without changing their ownership or ACL.
pub(crate) fn validate_path(path: &Path, directory: bool, private: bool) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    let absolute = std::path::absolute(path)?;
    let components: Vec<_> = absolute.ancestors().collect();
    let expected = current_sid()?;
    let mut held = Vec::new();
    for component in components.into_iter().rev() {
        let final_component = component == absolute;
        let file = std::fs::OpenOptions::new()
            .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(component)?;
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let needs_directory = !final_component || directory;
        if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || (information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != needs_directory
        {
            return Err(denied(
                "Private state paths must use ordinary directories and files without reparse points",
            ));
        }
        validate_acl(
            file.as_raw_handle(),
            &expected,
            final_component && private,
            !final_component,
        )?;
        // Retained ancestor handles prevent path substitution while descendant checks run.
        held.push(file);
    }
    Ok(())
}

fn validate_acl(handle: HANDLE, expected: &str, private: bool, ancestor: bool) -> io::Result<()> {
    let mut owner = std::ptr::null_mut();
    let mut acl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let error = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut acl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let _descriptor = LocalAllocation(descriptor);
    let owner = if owner.is_null() {
        String::new()
    } else {
        sid_string(owner)?
    };
    if (owner != expected && (private || !trusted_system_sid(&owner))) || acl.is_null() {
        return Err(denied(
            "Private state must be owned and access-controlled by the current Windows user",
        ));
    }
    for index in 0..unsafe { (*acl).AceCount } {
        let mut ace = std::ptr::null_mut();
        if unsafe { GetAce(acl, index as u32, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        // Private directories must also protect files that inherit their access rules.
        if (!private && header.AceFlags & 0x08 != 0) || header.AceType == 1 {
            continue;
        }
        if header.AceType != 0 {
            return Err(denied(
                "Private state has an unsupported access-control entry",
            ));
        }
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        let sid = sid_string(std::ptr::addr_of!(allowed.SidStart).cast_mut().cast())?;
        if sid == expected || trusted_system_sid(&sid) {
            continue;
        }
        // Public read access to the store does not grant authority over private state.
        use windows_sys::Win32::Foundation::{GENERIC_ALL, GENERIC_WRITE};
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA,
            FILE_WRITE_EA, WRITE_DAC, WRITE_OWNER,
        };
        let destructive = GENERIC_ALL | DELETE | FILE_DELETE_CHILD | WRITE_DAC | WRITE_OWNER;
        let write_mask = if ancestor {
            destructive
        } else {
            destructive
                | GENERIC_WRITE
                | FILE_APPEND_DATA
                | FILE_WRITE_ATTRIBUTES
                | FILE_WRITE_DATA
                | FILE_WRITE_EA
        };
        if private && allowed.Mask != 0 || allowed.Mask & write_mask != 0 {
            return Err(denied(
                "Private state grants access to another Windows user; use a private AGIT_HOME",
            ));
        }
    }
    Ok(())
}

fn trusted_system_sid(sid: &str) -> bool {
    matches!(
        sid,
        "S-1-5-18"
            | "S-1-5-32-544"
            | "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
    )
}

pub(crate) fn write_private_file(path: &Path, body: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ,
    };

    let parent = path
        .parent()
        .ok_or_else(|| denied("Private state needs a containing directory"))?;
    validate_path(parent, true, false)?;
    match std::fs::symlink_metadata(path) {
        Ok(_) => validate_path(path, false, true)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let sid = current_sid()?;
    let descriptor = private_descriptor(&sid, false)?;
    let attributes = attributes(&descriptor);
    let mut temporary =
        tempfile::Builder::new()
            .prefix(".private-")
            .make_in(parent, |candidate| {
                let name = wide(candidate)?;
                let handle = Handle::new(unsafe {
                    CreateFileW(
                        name.as_ptr(),
                        GENERIC_WRITE,
                        FILE_SHARE_READ | FILE_SHARE_DELETE,
                        &attributes,
                        CREATE_NEW,
                        FILE_FLAG_OPEN_REPARSE_POINT,
                        std::ptr::null_mut(),
                    )
                })?;
                let raw = handle.0;
                std::mem::forget(handle);
                Ok(unsafe { std::fs::File::from_raw_handle(raw) })
            })?;
    validate_acl(temporary.as_file().as_raw_handle(), &sid, true, false)?;
    temporary.write_all(body)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

pub(crate) fn private_directory(path: &Path) -> io::Result<()> {
    let descriptor = private_descriptor(&current_sid()?, true)?;
    let attributes = attributes(&descriptor);
    let name = wide(path)?;
    if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS as i32)
        {
            return Err(error);
        }
    }
    validate_path(path, true, true)
}
