//! Handle-relative Windows directory operations for plugin install and uninstall.
//!
//! Unix publication uses `openat` with `O_NOFOLLOW` so a directory entry cannot
//! be swapped for a symlink between lookup and use. Windows has no `openat`.
//! These helpers open each component with `FILE_OPEN_REPARSE_POINT` relative to
//! a held directory handle and refuse reparse points (symlinks, junctions, and
//! mount points) instead of following them.

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{self, Read},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr,
};

const FILE_LIST_DIRECTORY: u32 = 0x1;
const FILE_ADD_FILE: u32 = 0x2;
const FILE_ADD_SUBDIRECTORY: u32 = 0x4;
const FILE_READ_EA: u32 = 0x8;
const FILE_WRITE_EA: u32 = 0x10;
const FILE_TRAVERSE: u32 = 0x20;
const FILE_DELETE_CHILD: u32 = 0x40;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
const DELETE: u32 = 0x0001_0000;
const READ_CONTROL: u32 = 0x0002_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const FILE_GENERIC_READ: u32 =
    READ_CONTROL | FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | FILE_READ_EA | SYNCHRONIZE;
const FILE_GENERIC_WRITE: u32 =
    READ_CONTROL | FILE_ADD_FILE | FILE_WRITE_ATTRIBUTES | FILE_WRITE_EA | FILE_ADD_SUBDIRECTORY
        | SYNCHRONIZE;
const READ_DIRECTORY_ACCESS: u32 =
    READ_CONTROL | FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const DIRECTORY_ACCESS: u32 = READ_DIRECTORY_ACCESS
    | FILE_GENERIC_WRITE
    | FILE_DELETE_CHILD
    | DELETE
    | FILE_WRITE_ATTRIBUTES;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_OPEN: u32 = 1;
const FILE_CREATE: u32 = 2;
const FILE_DIRECTORY_FILE: u32 = 0x1;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
const FILE_OPEN_FOR_BACKUP_INTENT: u32 = 0x4000;
const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const OBJ_CASE_INSENSITIVE: u32 = 0x40;
const FILE_ATTRIBUTE_TAG_INFORMATION: i32 = 35;
const FILE_NAMES_INFORMATION: i32 = 12;
const FILE_RENAME_INFORMATION: i32 = 10;
const FILE_LINK_INFORMATION: i32 = 11;
const FILE_DISPOSITION_INFO: i32 = 4;
const STATUS_NO_MORE_FILES: i32 = 0x8000_0006u32 as i32;
const INVALID_HANDLE: isize = -1;
const DACL_SECURITY_INFORMATION: u32 = 0x4;
const SDDL_REVISION_1: u32 = 1;
const TOKEN_QUERY: u32 = 0x8;
const TOKEN_USER: i32 = 1;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0x0;
const INHERIT_ONLY_ACE: u8 = 0x8;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: isize,
    object_name: *const UnicodeString,
    attributes: u32,
    security_descriptor: *mut std::ffi::c_void,
    security_quality_of_service: *mut std::ffi::c_void,
}

#[repr(C)]
struct IoStatusBlock {
    status: isize,
    information: usize,
}

#[repr(C)]
struct FileAttributeTagInformation {
    file_attributes: u32,
    reparse_tag: u32,
}

#[repr(C)]
struct SecurityAttributes {
    length: u32,
    descriptor: *mut std::ffi::c_void,
    inherit_handle: i32,
}

#[repr(C)]
struct ByHandleFileInformation {
    file_attributes: u32,
    creation_low: u32,
    creation_high: u32,
    access_low: u32,
    access_high: u32,
    write_low: u32,
    write_high: u32,
    volume_serial: u32,
    size_high: u32,
    size_low: u32,
    links: u32,
    index_high: u32,
    index_low: u32,
}

#[repr(C)]
struct RenameHeader {
    replace_if_exists: u8,
    root_directory: isize,
    file_name_length: u32,
    file_name: u16,
}

const _: () = assert!(std::mem::offset_of!(RenameHeader, root_directory) == 8);
const _: () = assert!(std::mem::offset_of!(RenameHeader, file_name_length) == 16);
const _: () = assert!(std::mem::offset_of!(RenameHeader, file_name) == 20);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *const SecurityAttributes,
        disposition: u32,
        flags: u32,
        template: isize,
    ) -> isize;
    fn GetFileInformationByHandle(
        handle: isize,
        information: *mut ByHandleFileInformation,
    ) -> i32;
    fn FlushFileBuffers(handle: isize) -> i32;
    fn GetFinalPathNameByHandleW(
        handle: isize,
        buffer: *mut u16,
        size: u32,
        flags: u32,
    ) -> u32;
    fn SetFileInformationByHandle(
        handle: isize,
        class: i32,
        information: *const u8,
        size: u32,
    ) -> i32;
    fn LocalFree(memory: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn GetCurrentProcess() -> isize;
    fn OpenProcessToken(process: isize, access: u32, token: *mut isize) -> i32;
    fn CloseHandle(handle: isize) -> i32;
    fn GetTokenInformation(
        token: isize,
        class: i32,
        information: *mut u8,
        length: u32,
        returned: *mut u32,
    ) -> i32;
    fn CopySid(length: u32, destination: *mut u8, source: *const u8) -> i32;
    fn GetLengthSid(sid: *const u8) -> u32;
    fn EqualSid(left: *const u8, right: *const u8) -> i32;
    fn GetFileSecurityW(
        path: *const u16,
        information: u32,
        descriptor: *mut u8,
        length: u32,
        needed: *mut u32,
    ) -> i32;
    fn SetFileSecurityW(
        path: *const u16,
        information: u32,
        descriptor: *const std::ffi::c_void,
    ) -> i32;
    fn GetSecurityDescriptorDacl(
        descriptor: *const u8,
        present: *mut i32,
        acl: *mut *mut u8,
        defaulted: *mut i32,
    ) -> i32;
    fn GetAclInformation(
        acl: *const u8,
        information: *mut AclSizeInformation,
        length: u32,
        class: i32,
    ) -> i32;
    fn GetAce(acl: *const u8, index: u32, ace: *mut *mut u8) -> i32;
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(
        handle: *mut isize,
        access: u32,
        attributes: *const ObjectAttributes,
        io_status: *mut IoStatusBlock,
        allocation: *mut i64,
        file_attributes: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *mut std::ffi::c_void,
        ea_length: u32,
    ) -> i32;
    fn NtQueryInformationFile(
        handle: isize,
        io_status: *mut IoStatusBlock,
        information: *mut u8,
        length: u32,
        class: i32,
    ) -> i32;
    fn NtQueryDirectoryFile(
        handle: isize,
        event: isize,
        apc: isize,
        context: *mut std::ffi::c_void,
        io_status: *mut IoStatusBlock,
        information: *mut u8,
        length: u32,
        class: i32,
        single: u8,
        name: *const UnicodeString,
        restart: u8,
    ) -> i32;
    fn NtSetInformationFile(
        handle: isize,
        io_status: *mut IoStatusBlock,
        information: *const u8,
        length: u32,
        class: i32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        sddl: *const u16,
        revision: u32,
        descriptor: *mut *mut std::ffi::c_void,
        size: *mut u32,
    ) -> i32;
}

#[repr(C)]
struct AclSizeInformation {
    ace_count: u32,
    bytes_in_use: u32,
    bytes_free: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileId {
    pub volume: u32,
    pub index_high: u32,
    pub index_low: u32,
}

pub(crate) struct Directory {
    file: File,
}

impl Directory {
    fn from_handle(handle: OwnedHandle) -> Self {
        Self {
            file: File::from(handle),
        }
    }

    fn raw(&self) -> isize {
        self.file.as_raw_handle() as isize
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        flush(self.raw())
    }

    pub(crate) fn file_id(&self) -> io::Result<FileId> {
        file_id_of(&self.file)
    }

    pub(crate) fn final_path(&self) -> io::Result<PathBuf> {
        final_path(self.raw())
    }

    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
        })
    }
}

pub(crate) fn ensure_component(name: &OsStr) -> io::Result<()> {
    if name.is_empty() || name.encode_wide().any(|unit| unit == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "plugin path component is empty",
        ));
    }
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(part)), None) if part == name => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "plugin path component is not a single name",
            ));
        }
    }
    let text = name.to_string_lossy();
    if text.contains(['/', '\\', ':']) || text.ends_with([' ', '.']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "plugin path component is not a single name",
        ));
    }
    Ok(())
}

pub(crate) fn open_directory(path: &Path) -> io::Result<Directory> {
    open_directory_access(path, false)
}

pub(crate) fn open_writable_directory(path: &Path) -> io::Result<Directory> {
    open_directory_access(path, true)
}

fn open_directory_access(path: &Path, writable_final: bool) -> io::Result<Directory> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "plugin directory path is empty",
        ));
    }
    let mut current = if path.is_absolute() {
        open_volume_root(path)?
    } else {
        open_relative_root()?
    };
    let normals: Vec<&OsStr> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            Component::Prefix(_) | Component::RootDir | Component::CurDir => None,
            Component::ParentDir => Some(OsStr::new("..")),
        })
        .collect();
    if normals.iter().any(|name| *name == "..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "plugin directory path cannot contain parent components",
        ));
    }
    for (index, name) in normals.iter().enumerate() {
        let writable = writable_final && index + 1 == normals.len();
        current = open_child_dir(&current, name, writable)?;
    }
    Ok(current)
}

pub(crate) fn open_subdir(parent: &Directory, name: &OsStr) -> io::Result<Directory> {
    open_child_dir(parent, name, false)
}

pub(crate) fn open_writable_subdir(parent: &Directory, name: &OsStr) -> io::Result<Directory> {
    open_child_dir(parent, name, true)
}

fn open_child_dir(parent: &Directory, name: &OsStr, writable: bool) -> io::Result<Directory> {
    ensure_component(name)?;
    let handle = nt_open(
        parent.raw(),
        &wide_name(name)?,
        if writable {
            DIRECTORY_ACCESS
        } else {
            READ_DIRECTORY_ACCESS
        },
        0,
        FILE_SHARE_ALL,
        FILE_OPEN,
        FILE_DIRECTORY_FILE
            | FILE_SYNCHRONOUS_IO_NONALERT
            | FILE_OPEN_REPARSE_POINT
            | FILE_OPEN_FOR_BACKUP_INTENT,
    )?;
    reject_reparse(handle.as_raw_handle() as isize)?;
    Ok(Directory::from_handle(handle))
}

pub(crate) fn open_file(parent: &Directory, name: &OsStr) -> io::Result<File> {
    ensure_component(name)?;
    let handle = nt_open(
        parent.raw(),
        &wide_name(name)?,
        FILE_GENERIC_READ,
        0,
        FILE_SHARE_ALL,
        FILE_OPEN,
        FILE_NON_DIRECTORY_FILE
            | FILE_SYNCHRONOUS_IO_NONALERT
            | FILE_OPEN_REPARSE_POINT
            | FILE_OPEN_FOR_BACKUP_INTENT,
    )?;
    reject_reparse(handle.as_raw_handle() as isize)?;
    Ok(File::from(handle))
}

pub(crate) fn create_dir(parent: &Directory, name: &OsStr) -> io::Result<Directory> {
    ensure_component(name)?;
    let handle = nt_open(
        parent.raw(),
        &wide_name(name)?,
        DIRECTORY_ACCESS,
        FILE_ATTRIBUTE_NORMAL,
        FILE_SHARE_ALL,
        FILE_CREATE,
        FILE_DIRECTORY_FILE
            | FILE_SYNCHRONOUS_IO_NONALERT
            | FILE_OPEN_REPARSE_POINT
            | FILE_OPEN_FOR_BACKUP_INTENT,
    )?;
    reject_reparse(handle.as_raw_handle() as isize)?;
    Ok(Directory::from_handle(handle))
}

pub(crate) fn create_file(parent: &Directory, name: &OsStr) -> io::Result<File> {
    ensure_component(name)?;
    let handle = nt_open(
        parent.raw(),
        &wide_name(name)?,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE,
        FILE_ATTRIBUTE_NORMAL,
        FILE_SHARE_ALL,
        FILE_CREATE,
        FILE_NON_DIRECTORY_FILE
            | FILE_SYNCHRONOUS_IO_NONALERT
            | FILE_OPEN_REPARSE_POINT
            | FILE_OPEN_FOR_BACKUP_INTENT,
    )?;
    Ok(File::from(handle))
}

pub(crate) fn read_names(parent: &Directory) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    let mut buffer = vec![0u64; 8192];
    let mut restart = 1u8;
    loop {
        let mut io_status = IoStatusBlock {
            status: 0,
            information: 0,
        };
        let status = unsafe {
            NtQueryDirectoryFile(
                parent.raw(),
                0,
                0,
                ptr::null_mut(),
                &mut io_status,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
                FILE_NAMES_INFORMATION,
                0,
                ptr::null(),
                restart,
            )
        };
        restart = 0;
        if status == STATUS_NO_MORE_FILES {
            break;
        }
        if status < 0 {
            return Err(nt_error(status));
        }
        let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), buffer.len() * 8) };
        let mut offset = 0usize;
        loop {
            if offset + 12 > bytes.len() {
                return Err(io::Error::other("plugin directory listing was truncated"));
            }
            let next = u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            let name_bytes =
                u32::from_ne_bytes(bytes[offset + 8..offset + 12].try_into().unwrap()) as usize;
            let name_at = offset + 12;
            if name_bytes % 2 != 0 || name_at + name_bytes > bytes.len() {
                return Err(io::Error::other("plugin directory listing was truncated"));
            }
            let units = name_bytes / 2;
            let mut wide = Vec::with_capacity(units);
            for index in 0..units {
                let start = name_at + index * 2;
                wide.push(u16::from_ne_bytes(
                    bytes[start..start + 2].try_into().unwrap(),
                ));
            }
            let name = OsString::from_wide(&wide);
            if name != "." && name != ".." {
                names.push(name);
            }
            if next == 0 {
                break;
            }
            if next > bytes.len() - offset {
                return Err(io::Error::other("plugin directory listing was truncated"));
            }
            offset += next;
        }
    }
    Ok(names)
}

pub(crate) fn read_limited(parent: &Directory, name: &OsStr, limit: u64) -> io::Result<Vec<u8>> {
    let file = open_file(parent, name)?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "plugin file exceeds its read limit",
        ));
    }
    Ok(bytes)
}

pub(crate) fn rename_noreplace(
    source_parent: &Directory,
    source_name: &OsStr,
    destination_parent: &Directory,
    destination_name: &OsStr,
) -> io::Result<()> {
    ensure_component(source_name)?;
    ensure_component(destination_name)?;
    let source = nt_open(
        source_parent.raw(),
        &wide_name(source_name)?,
        DELETE | SYNCHRONIZE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
        0,
        FILE_SHARE_ALL,
        FILE_OPEN,
        FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT | FILE_OPEN_FOR_BACKUP_INTENT,
    )?;
    reject_reparse(source.as_raw_handle() as isize)?;
    set_name_information(
        source.as_raw_handle() as isize,
        destination_parent.raw(),
        destination_name,
        FILE_RENAME_INFORMATION,
    )
}

pub(crate) fn hard_link(
    source_parent: &Directory,
    source_name: &OsStr,
    destination_parent: &Directory,
    destination_name: &OsStr,
) -> io::Result<()> {
    ensure_component(source_name)?;
    ensure_component(destination_name)?;
    let source = nt_open(
        source_parent.raw(),
        &wide_name(source_name)?,
        FILE_GENERIC_READ | FILE_WRITE_ATTRIBUTES,
        0,
        FILE_SHARE_ALL,
        FILE_OPEN,
        FILE_NON_DIRECTORY_FILE
            | FILE_SYNCHRONOUS_IO_NONALERT
            | FILE_OPEN_REPARSE_POINT
            | FILE_OPEN_FOR_BACKUP_INTENT,
    )?;
    reject_reparse(source.as_raw_handle() as isize)?;
    set_name_information(
        source.as_raw_handle() as isize,
        destination_parent.raw(),
        destination_name,
        FILE_LINK_INFORMATION,
    )
}

pub(crate) fn remove_file(parent: &Directory, name: &OsStr) -> io::Result<()> {
    delete_name(parent, name, false)
}

pub(crate) fn remove_dir(parent: &Directory, name: &OsStr) -> io::Result<()> {
    delete_name(parent, name, true)
}

pub(crate) fn remove_tree(directory: &Directory) -> io::Result<()> {
    for name in read_names(directory)? {
        let os_name = name.as_os_str();
        match open_unknown(directory, os_name) {
            Ok(Opened::Directory(_)) => {
                let child = open_child_dir(directory, os_name, true)?;
                remove_tree(&child)?;
                remove_dir(directory, os_name)?;
            }
            Ok(Opened::Leaf) => remove_file(directory, os_name)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    directory.sync()
}

pub(crate) fn file_id_of(file: &File) -> io::Result<FileId> {
    let mut information = ByHandleFileInformation {
        file_attributes: 0,
        creation_low: 0,
        creation_high: 0,
        access_low: 0,
        access_high: 0,
        write_low: 0,
        write_high: 0,
        volume_serial: 0,
        size_high: 0,
        size_low: 0,
        links: 0,
        index_high: 0,
        index_low: 0,
    };
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as isize, &mut information) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileId {
        volume: information.volume_serial,
        index_high: information.index_high,
        index_low: information.index_low,
    })
}

pub(crate) fn rename_unsupported(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(1 | 50 | 120))
}

pub(crate) fn restrict_to_current_user(path: &Path) -> io::Result<()> {
    let (descriptor, _guard) = user_security_descriptor()?;
    let wide = wide_path(path)?;
    let ok = unsafe { SetFileSecurityW(wide.as_ptr(), DACL_SECURITY_INFORMATION, descriptor) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn create_private_file(path: &Path) -> io::Result<File> {
    let (descriptor, _guard) = user_security_descriptor()?;
    let attributes = SecurityAttributes {
        length: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor,
        inherit_handle: 0,
    };
    let wide = wide_path(path)?;
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            0,
            &attributes,
            1,
            FILE_ATTRIBUTE_NORMAL,
            0,
        )
    };
    owned_handle(handle).map(File::from)
}

pub(crate) fn is_restricted_to_current_user(path: &Path) -> io::Result<bool> {
    let wide = wide_path(path)?;
    let mut needed = 0u32;
    unsafe {
        GetFileSecurityW(
            wide.as_ptr(),
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0u8; needed as usize];
    let ok = unsafe {
        GetFileSecurityW(
            wide.as_ptr(),
            DACL_SECURITY_INFORMATION,
            buffer.as_mut_ptr(),
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut present = 0i32;
    let mut defaulted = 0i32;
    let mut acl = ptr::null_mut();
    let ok = unsafe {
        GetSecurityDescriptorDacl(buffer.as_ptr(), &mut present, &mut acl, &mut defaulted)
    };
    if ok == 0 || present == 0 || acl.is_null() {
        return Ok(false);
    }
    let mut size = AclSizeInformation {
        ace_count: 0,
        bytes_in_use: 0,
        bytes_free: 0,
    };
    let ok = unsafe { GetAclInformation(acl, &mut size, std::mem::size_of_val(&size) as u32, 2) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let current = current_user_sid()?;
    let mut allowed_current = false;
    for index in 0..size.ace_count {
        let mut ace = ptr::null_mut();
        let ok = unsafe { GetAce(acl, index, &mut ace) };
        if ok == 0 || ace.is_null() {
            return Err(io::Error::last_os_error());
        }
        let header = unsafe { &*(ace as *const AceHeader) };
        if header.ace_flags & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        if header.ace_type != ACCESS_ALLOWED_ACE_TYPE || header.ace_size < 8 {
            return Ok(false);
        }
        let sid = unsafe { ace.add(8) };
        let equal = unsafe { EqualSid(sid, current.as_ptr()) };
        if equal == 0 {
            return Ok(false);
        }
        allowed_current = true;
    }
    Ok(allowed_current)
}

enum Opened {
    Directory(Directory),
    Leaf,
}

#[repr(C)]
struct AceHeader {
    ace_type: u8,
    ace_flags: u8,
    ace_size: u16,
}

struct LocalFreeGuard(*mut std::ffi::c_void);
impl Drop for LocalFreeGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

fn open_unknown(parent: &Directory, name: &OsStr) -> io::Result<Opened> {
    ensure_component(name)?;
    let handle = nt_open(
        parent.raw(),
        &wide_name(name)?,
        READ_DIRECTORY_ACCESS,
        0,
        FILE_SHARE_ALL,
        FILE_OPEN,
        FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT | FILE_OPEN_FOR_BACKUP_INTENT,
    )?;
    let raw = handle.as_raw_handle() as isize;
    if is_reparse(raw)? {
        return Ok(Opened::Leaf);
    }
    let attributes = file_attributes(raw)?;
    if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        Ok(Opened::Directory(Directory::from_handle(handle)))
    } else {
        Ok(Opened::Leaf)
    }
}

fn delete_name(parent: &Directory, name: &OsStr, directory: bool) -> io::Result<()> {
    ensure_component(name)?;
    let mut options =
        FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT | FILE_OPEN_FOR_BACKUP_INTENT;
    if directory {
        options |= FILE_DIRECTORY_FILE;
    }
    let handle = nt_open(
        parent.raw(),
        &wide_name(name)?,
        DELETE | SYNCHRONIZE | FILE_READ_ATTRIBUTES,
        0,
        FILE_SHARE_ALL,
        FILE_OPEN,
        options,
    )?;
    let delete = [1u8];
    let ok = unsafe {
        SetFileInformationByHandle(
            handle.as_raw_handle() as isize,
            FILE_DISPOSITION_INFO,
            delete.as_ptr(),
            delete.len() as u32,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn set_name_information(
    source: isize,
    destination_parent: isize,
    destination_name: &OsStr,
    class: i32,
) -> io::Result<()> {
    let wide = wide_name(destination_name)?;
    let name_bytes = wide.len() * 2;
    if name_bytes > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "plugin path component is too long",
        ));
    }
    let total = std::mem::offset_of!(RenameHeader, file_name) + name_bytes;
    let mut buffer = vec![0u8; total];
    buffer[8..16].copy_from_slice(&destination_parent.to_ne_bytes());
    buffer[16..20].copy_from_slice(&(name_bytes as u32).to_ne_bytes());
    for (index, unit) in wide.iter().enumerate() {
        let start = 20 + index * 2;
        buffer[start..start + 2].copy_from_slice(&unit.to_ne_bytes());
    }
    let mut io_status = IoStatusBlock {
        status: 0,
        information: 0,
    };
    let status = unsafe {
        NtSetInformationFile(
            source,
            &mut io_status,
            buffer.as_ptr(),
            buffer.len() as u32,
            class,
        )
    };
    if status < 0 {
        Err(nt_error(status))
    } else {
        Ok(())
    }
}

fn open_volume_root(path: &Path) -> io::Result<Directory> {
    let wide = volume_root(path)?;
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            READ_DIRECTORY_ACCESS,
            FILE_SHARE_ALL,
            ptr::null(),
            3,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            0,
        )
    };
    let owned = owned_handle(handle)?;
    reject_reparse(owned.as_raw_handle() as isize)?;
    Ok(Directory::from_handle(owned))
}

fn open_relative_root() -> io::Result<Directory> {
    let name: Vec<u16> = ['.' as u16, 0].to_vec();
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            READ_DIRECTORY_ACCESS,
            FILE_SHARE_ALL,
            ptr::null(),
            3,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            0,
        )
    };
    let owned = owned_handle(handle)?;
    reject_reparse(owned.as_raw_handle() as isize)?;
    Ok(Directory::from_handle(owned))
}

fn volume_root(path: &Path) -> io::Result<Vec<u16>> {
    let prefix = path.components().find_map(|component| match component {
        Component::Prefix(prefix) => Some(prefix),
        _ => None,
    });
    let Some(prefix) = prefix else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "plugin directory path must be absolute",
        ));
    };
    let text = match prefix.kind() {
        Prefix::Disk(disk) | Prefix::VerbatimDisk(disk) => {
            format!(r"\\?\{}:\", disk as char)
        }
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
            format!(
                r"\\?\UNC\{}\{}\",
                server.to_string_lossy(),
                share.to_string_lossy()
            )
        }
        Prefix::Verbatim(_) | Prefix::DeviceNS(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "plugin directory device path is not supported",
            ));
        }
    };
    Ok(text.encode_utf16().chain(std::iter::once(0)).collect())
}

fn nt_open(
    root: isize,
    name: &[u16],
    access: u32,
    attributes: u32,
    share: u32,
    disposition: u32,
    options: u32,
) -> io::Result<OwnedHandle> {
    if name.len() > (u16::MAX as usize) / 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "plugin path component is too long",
        ));
    }
    let mut wide = name.to_vec();
    let name_string = UnicodeString {
        length: (wide.len() * 2) as u16,
        maximum_length: (wide.len() * 2) as u16,
        buffer: wide.as_mut_ptr(),
    };
    let object = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as u32,
        root_directory: root,
        object_name: &name_string,
        attributes: OBJ_CASE_INSENSITIVE,
        security_descriptor: ptr::null_mut(),
        security_quality_of_service: ptr::null_mut(),
    };
    let mut handle = 0isize;
    let mut io_status = IoStatusBlock {
        status: 0,
        information: 0,
    };
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &object,
            &mut io_status,
            ptr::null_mut(),
            attributes,
            share,
            disposition,
            options,
            ptr::null_mut(),
            0,
        )
    };
    if status < 0 {
        return Err(nt_error(status));
    }
    owned_handle(handle)
}

const REPARSE_REFUSAL: &str = "plugin path encountered a reparse point";
const ERROR_STOPPED_ON_SYMLINK: i32 = 681;

pub(crate) fn is_reparse_refusal(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_STOPPED_ON_SYMLINK)
        || (error.kind() == io::ErrorKind::Unsupported
            && error
                .get_ref()
                .is_some_and(|source| source.to_string() == REPARSE_REFUSAL))
}

fn reject_reparse(handle: isize) -> io::Result<()> {
    if is_reparse(handle)? {
        Err(io::Error::new(io::ErrorKind::Unsupported, REPARSE_REFUSAL))
    } else {
        Ok(())
    }
}

fn is_reparse(handle: isize) -> io::Result<bool> {
    Ok(file_attributes(handle)? & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

fn file_attributes(handle: isize) -> io::Result<u32> {
    let mut information = FileAttributeTagInformation {
        file_attributes: 0,
        reparse_tag: 0,
    };
    let mut io_status = IoStatusBlock {
        status: 0,
        information: 0,
    };
    let status = unsafe {
        NtQueryInformationFile(
            handle,
            &mut io_status,
            (&mut information as *mut FileAttributeTagInformation).cast(),
            std::mem::size_of::<FileAttributeTagInformation>() as u32,
            FILE_ATTRIBUTE_TAG_INFORMATION,
        )
    };
    if status < 0 {
        Err(nt_error(status))
    } else {
        Ok(information.file_attributes)
    }
}

fn final_path(handle: isize) -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 1024];
    let mut length = unsafe { GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0) };
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize >= buffer.len() {
        buffer.resize(length as usize + 1, 0);
        length = unsafe { GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0) };
        if length == 0 || length as usize >= buffer.len() {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(PathBuf::from(OsString::from_wide(&buffer[..length as usize])))
}

fn flush(handle: isize) -> io::Result<()> {
    let ok = unsafe { FlushFileBuffers(handle) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn owned_handle(handle: isize) -> io::Result<OwnedHandle> {
    if handle == 0 || handle == INVALID_HANDLE {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(handle as *mut std::ffi::c_void) })
    }
}

fn wide_name(name: &OsStr) -> io::Result<Vec<u16>> {
    ensure_component(name)?;
    Ok(name.encode_wide().collect())
}

fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "plugin path is empty",
        ));
    }
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.iter().any(|unit| *unit == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "plugin path contains NUL",
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn nt_error(status: i32) -> io::Error {
    let code = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(if code == 0 { 31 } else { code as i32 })
}

fn user_security_descriptor() -> io::Result<(*mut std::ffi::c_void, LocalFreeGuard)> {
    let sddl: Vec<u16> = "D:P(A;OICI;FA;;;CU)"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut descriptor = ptr::null_mut();
    let mut size = 0u32;
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            &mut size,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok((descriptor, LocalFreeGuard(descriptor)))
    }
}

fn current_user_sid() -> io::Result<Vec<u8>> {
    let mut token = 0isize;
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return Err(io::Error::last_os_error());
    }
    let _close = TokenGuard(token);
    let mut needed = 0u32;
    unsafe {
        GetTokenInformation(token, TOKEN_USER, ptr::null_mut(), 0, &mut needed);
    }
    if needed < 16 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0u8; needed as usize];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TOKEN_USER,
            buffer.as_mut_ptr(),
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let sid_pointer = usize::from_ne_bytes(buffer[..std::mem::size_of::<usize>()].try_into().unwrap()) as *const u8;
    if sid_pointer.is_null() {
        return Err(io::Error::other("current user token has no SID"));
    }
    let length = unsafe { GetLengthSid(sid_pointer) };
    if length == 0 || length > 256 {
        return Err(io::Error::other("current user SID is invalid"));
    }
    let mut sid = vec![0u8; length as usize];
    let copied = unsafe { CopySid(length, sid.as_mut_ptr(), sid_pointer) };
    if copied == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(sid)
    }
}

struct TokenGuard(isize);
impl Drop for TokenGuard {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn noreplace_rename_keeps_existing_bytes_and_rejects_parent_components() {
        let root = tempfile::tempdir().unwrap();
        let parent = open_writable_directory(root.path()).unwrap();
        let source = create_dir(&parent, OsStr::new("staging")).unwrap();
        let mut file = create_file(&source, OsStr::new("plugin.json")).unwrap();
        file.write_all(b"verified").unwrap();
        file.sync_all().unwrap();
        source.sync().unwrap();
        create_dir(&parent, OsStr::new("plugin")).unwrap();
        let error = rename_noreplace(
            &parent,
            OsStr::new("staging"),
            &parent,
            OsStr::new("plugin"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(!root.path().join("plugin/plugin.json").exists());
        assert_eq!(
            std::fs::read(root.path().join("staging/plugin.json")).unwrap(),
            b"verified"
        );
        std::fs::remove_dir(root.path().join("plugin")).unwrap();
        rename_noreplace(
            &parent,
            OsStr::new("staging"),
            &parent,
            OsStr::new("plugin"),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("plugin/plugin.json")).unwrap(),
            b"verified"
        );
        assert!(ensure_component(OsStr::new("..")).is_err());
        assert!(ensure_component(OsStr::new(r"nested\name")).is_err());
        assert!(open_directory(&root.path().join("missing")).is_err());
    }
}
