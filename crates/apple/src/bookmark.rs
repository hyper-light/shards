//! A directory's bookmark, for another process to resolve: what the VM process in App
//! Sandbox is granted a directory by (D30). CFURL.h: options 0 grant "access to the
//! resource to a process that resolves the bookmark".

use std::path::Path;

use crate::frameworks::{self, CFTypeRef, Owned};

/// A read-write bookmark for directory `path`.
pub fn bookmark(path: &Path) -> Result<Vec<u8>, String> {
    use std::os::unix::ffi::OsStrExt;
    let at = |what: &str| format!("{}: {what}", path.display());
    let api = frameworks::api().map_err(|e| at(&e))?;
    let bytes = path.as_os_str().as_bytes();
    let len = isize::try_from(bytes.len()).map_err(|_| at("a path too long"))?;
    // SAFETY: a buffer of `len` bytes; the URL is ours.
    let url = Owned::new(api, unsafe {
        (api.CFURLCreateFromFileSystemRepresentation)(std::ptr::null(), bytes.as_ptr(), len, 1)
    })
    .ok_or_else(|| at("no URL for it"))?;
    let mut error: CFTypeRef = std::ptr::null();
    // SAFETY: a URL we hold; the data and the error, if made, are ours.
    let data = unsafe {
        (api.CFURLCreateBookmarkData)(
            std::ptr::null(),
            url.ptr,
            0,
            std::ptr::null(),
            std::ptr::null(),
            &mut error,
        )
    };
    drop(Owned::new(api, error));
    let data = Owned::new(api, data).ok_or_else(|| at("no bookmark for it"))?;
    // SAFETY: CFData's bytes, valid while `data` is, copied.
    let out = unsafe {
        let n = usize::try_from((api.CFDataGetLength)(data.ptr)).unwrap_or(0);
        std::slice::from_raw_parts((api.CFDataGetBytePtr)(data.ptr), n).to_vec()
    };
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[test]
    fn a_directory_has_a_bookmark() {
        let dir = std::env::temp_dir();
        let b = super::bookmark(&dir).unwrap();
        assert!(!b.is_empty());
        assert!(super::bookmark(std::path::Path::new("/no/such/place/at/all")).is_err());
    }
}
