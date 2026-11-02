//! Loading the NVIDIA driver libraries (NVDEC, NVENC and CUDA).
//!
//! Names passed here must be plain file names. On Windows a bare `LoadLibrary` also searches the
//! application directory, the current directory and `PATH`, so a DLL dropped next to a
//! double-clicked `ironav.exe` would run inside the process (CWE-427, ATT&CK T1574.001). The NVIDIA
//! driver installs these DLLs in `System32`, so the Windows load restricts the search to that
//! directory. On Linux the names are sonames resolved by the dynamic loader; the loader's own search
//! path (including `LD_LIBRARY_PATH`, which the operator controls) applies, and nothing here adds a
//! relative or environment-derived path.

use libloading::Library;

/// True for a plain file name: no directory part, no drive prefix, no control characters, and not
/// `.` or `..`. Only such names reach the loader.
pub(crate) fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(|c: char| c == '/' || c == '\\' || c == ':' || c.is_control())
}

/// Opens the first of `names` that loads. On failure the error names the last candidate tried.
pub(crate) fn load(names: &[&str]) -> std::result::Result<Library, String> {
    let mut last = String::new();
    for &n in names {
        if !is_plain_file_name(n) {
            last = format!("{n:?}: not a plain library file name");
            continue;
        }
        match open(n) {
            Ok(l) => return Ok(l),
            Err(e) => last = format!("{n}: {e}"),
        }
    }
    Err(last)
}

#[cfg(windows)]
fn open(name: &str) -> std::result::Result<Library, libloading::Error> {
    use libloading::os::windows::{Library as SystemLibrary, LOAD_LIBRARY_SEARCH_SYSTEM32};
    // SAFETY: loads a NVIDIA driver library; its initialisers are the vendor's. LOAD_LIBRARY_SEARCH_SYSTEM32
    // limits the search for `name` to %SystemRoot%\System32, so the application directory, the current
    // directory and PATH are not consulted.
    unsafe { SystemLibrary::load_with_flags(name, LOAD_LIBRARY_SEARCH_SYSTEM32) }.map(Library::from)
}

#[cfg(not(windows))]
fn open(name: &str) -> std::result::Result<Library, libloading::Error> {
    // SAFETY: loads a NVIDIA driver library by soname; its initialisers are the vendor's.
    unsafe { Library::new(name) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_names_are_plain_file_names() {
        for ok in ["nvcuda.dll", "nvcuvid.dll", "nvEncodeAPI64.dll", "libcuda.so.1", "libnvcuvid.so"] {
            assert!(is_plain_file_name(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "..\\nvcuda.dll",
            "../libcuda.so.1",
            "C:nvcuda.dll",
            "C:\\temp\\nvcuda.dll",
            "\\\\server\\share\\nvcuda.dll",
            "/tmp/libcuda.so.1",
            "nv\0cuda.dll",
            "nvcuda\n.dll",
        ] {
            assert!(!is_plain_file_name(bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn paths_are_refused_without_loading() {
        // Refusal happens before the loader is called, so these names cannot reach it.
        let err = load(&["..\\nvcuda.dll", "C:\\temp\\nvcuda.dll"]).unwrap_err();
        assert!(err.contains("not a plain library file name"), "{err}");
    }
}
