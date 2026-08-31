use std::{ffi::c_void, mem, ptr};

fn mallctl<T: Copy>(name: &'static [u8], new_value: Option<T>) -> Result<T, ()> {
    debug_assert_eq!(name.last(), Some(&0));
    let mut previous = mem::MaybeUninit::<T>::uninit();
    let mut previous_length = mem::size_of::<T>();
    let mut new_value = new_value;
    let (new_pointer, new_length) = match new_value.as_mut() {
        Some(value) => (ptr::from_mut(value).cast::<c_void>(), mem::size_of::<T>()),
        None => (ptr::null_mut(), 0),
    };
    // SAFETY: Every call site fixes `T` to the type documented by jemalloc for
    // the static, NUL-terminated key. Buffers remain valid for this call.
    let status = unsafe {
        tikv_jemalloc_sys::mallctl(
            name.as_ptr().cast(),
            previous.as_mut_ptr().cast(),
            &mut previous_length,
            new_pointer,
            new_length,
        )
    };
    if status != 0 || previous_length != mem::size_of::<T>() {
        return Err(());
    }
    // SAFETY: A successful mallctl initialized exactly one value of T.
    Ok(unsafe { previous.assume_init() })
}

pub(crate) fn read_usize(name: &'static [u8]) -> Option<usize> {
    mallctl::<usize>(name, None).ok()
}

pub(crate) fn read_bool(name: &'static [u8]) -> Option<bool> {
    mallctl::<bool>(name, None).ok()
}

pub(crate) fn update_bool(name: &'static [u8], value: bool) -> Result<bool, ()> {
    mallctl(name, Some(value))
}

pub(crate) fn write_pointer(name: &'static [u8], value: *const std::ffi::c_char) -> Result<(), ()> {
    debug_assert_eq!(name.last(), Some(&0));
    let mut value = value;
    // SAFETY: `prof.dump` consumes the pointed-to NUL-terminated path during
    // this call, and the caller keeps its CString alive until return.
    let status = unsafe {
        tikv_jemalloc_sys::mallctl(
            name.as_ptr().cast(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::from_mut(&mut value).cast(),
            mem::size_of::<*const std::ffi::c_char>(),
        )
    };
    (status == 0).then_some(()).ok_or(())
}

pub(crate) fn advance_epoch() -> Result<(), ()> {
    mallctl::<u64>(b"epoch\0", Some(1)).map(|_| ())
}
