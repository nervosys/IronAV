//! Resource limits for sidecar children: `max_memory_mb` and `max_cpu_seconds` in a sidecar's entry
//! (see docs/sidecars.md). Unset means no limit, as before.
//!
//! On Unix the child's limits are set with `setrlimit` between `fork` and `exec`: an address-space
//! limit (`RLIMIT_AS`) and a CPU-time limit (`RLIMIT_CPU`). If they cannot be set the program does
//! not run. On Windows a job object holds the child: a per-process memory limit, a per-process
//! user-mode CPU-time limit, and kill-on-close, so the child cannot outlive the job's handle. The
//! job is assigned once the child has started, so a child that starts a grandchild in the first
//! instant is not limited for that instant; a failed assignment kills the child.

use std::io;
use std::process::{Child, Command};

/// The largest memory limit accepted: 1 TiB.
pub const MAX_MEMORY_MB: f64 = 1_048_576.0;
/// The largest CPU-time limit accepted: a year of CPU time.
pub const MAX_CPU_SECONDS: f64 = 365.0 * 86_400.0;

/// The limits of one sidecar, as the operator set them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Limits {
    pub memory_mb: Option<f64>,
    pub cpu_seconds: Option<f64>,
}

impl Limits {
    pub fn is_none(&self) -> bool {
        self.memory_mb.is_none() && self.cpu_seconds.is_none()
    }
}

/// Whether `v` is a memory limit IronAV accepts: a whole number of MB from 1 to [`MAX_MEMORY_MB`].
pub fn valid_memory_mb(v: f64) -> bool {
    (1.0..=MAX_MEMORY_MB).contains(&v) && v.fract() == 0.0
}

/// Whether `v` is a CPU-time limit IronAV accepts: above 0 and at most [`MAX_CPU_SECONDS`].
pub fn valid_cpu_seconds(v: f64) -> bool {
    v > 0.0 && v <= MAX_CPU_SECONDS
}

/// What keeps the limits in force for as long as the child runs (on Windows, the job object).
#[cfg(windows)]
pub struct Guard {
    _job: Option<Job>,
}

/// What keeps the limits in force for as long as the child runs (nothing needs keeping on Unix).
#[cfg(not(windows))]
pub struct Guard;

/// Sets up the limits the child's program will run under. Call before `spawn`.
#[cfg(unix)]
pub fn before_spawn(cmd: &mut Command, l: &Limits) {
    use std::os::unix::process::CommandExt;
    if l.is_none() {
        return;
    }
    // whole MB, checked by the config loader; a CPU limit rounds up to the second
    let as_bytes = l.memory_mb.map(|mb| (mb as u64) << 20);
    let cpu = l.cpu_seconds.map(|s| s.ceil() as u64);
    // SAFETY: the closure makes two setrlimit calls and nothing else. Both are async-signal-safe,
    // and the closure allocates nothing.
    unsafe {
        cmd.pre_exec(move || {
            if let Some(bytes) = as_bytes {
                let r = libc::rlimit { rlim_cur: bytes as libc::rlim_t, rlim_max: bytes as libc::rlim_t };
                if libc::setrlimit(libc::RLIMIT_AS, &r) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if let Some(secs) = cpu {
                let r = libc::rlimit { rlim_cur: secs as libc::rlim_t, rlim_max: secs as libc::rlim_t };
                if libc::setrlimit(libc::RLIMIT_CPU, &r) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// Sets up the limits the child's program will run under. Call before `spawn`.
#[cfg(windows)]
pub fn before_spawn(_cmd: &mut Command, _l: &Limits) {
    // on Windows the limits come from a job object, assigned once the child exists
}

/// Applies the limits to a child that has started. An error means they are not in force; the
/// caller must then stop the child.
#[cfg(unix)]
pub fn after_spawn(_child: &Child, _l: &Limits) -> io::Result<Guard> {
    Ok(Guard)
}

/// Applies the limits to a child that has started. An error means they are not in force; the
/// caller must then stop the child.
#[cfg(windows)]
pub fn after_spawn(child: &Child, l: &Limits) -> io::Result<Guard> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME,
    };

    if l.is_none() {
        return Ok(Guard { _job: None });
    }
    let fail = |e: windows::core::Error| io::Error::other(e.to_string());
    // SAFETY: a new unnamed job with default security; the handle is owned by `job` from here on,
    // so it is closed on every path out of this function.
    let handle = unsafe { CreateJobObjectW(None, windows::core::PCWSTR::null()) }.map_err(fail)?;
    let job = Job(handle.0 as usize);

    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let mut flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE.0;
    if let Some(mb) = l.memory_mb {
        let bytes = (mb as u64) << 20;
        info.ProcessMemoryLimit = usize::try_from(bytes).map_err(|_| io::Error::other("max_memory_mb is more than this platform can address"))?;
        flags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY.0;
    }
    if let Some(s) = l.cpu_seconds {
        // the CPU limit is in 100 ns units
        info.BasicLimitInformation.PerProcessUserTimeLimit = (s * 1e7).ceil() as i64;
        flags |= JOB_OBJECT_LIMIT_PROCESS_TIME.0;
    }
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT(flags);
    // SAFETY: `info` is a complete JOBOBJECT_EXTENDED_LIMIT_INFORMATION and the length passed is its size.
    unsafe {
        SetInformationJobObject(
            HANDLE(job.0 as *mut _),
            JobObjectExtendedLimitInformation,
            &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .map_err(fail)?;
    // SAFETY: both handles are valid: the job was just created, and the child's handle is live
    // while the child has not been waited for.
    unsafe { AssignProcessToJobObject(HANDLE(job.0 as *mut _), HANDLE(child.as_raw_handle() as *mut _)) }.map_err(fail)?;
    Ok(Guard { _job: Some(job) })
}

/// A job object's handle, closed when dropped (which kills whatever is still in the job).
#[cfg(windows)]
struct Job(usize);

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateJobObjectW and is closed only here.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(windows::Win32::Foundation::HANDLE(self.0 as *mut _)) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_limits_are_whole_megabytes_within_a_tib() {
        assert!(valid_memory_mb(1.0));
        assert!(valid_memory_mb(4096.0));
        assert!(valid_memory_mb(MAX_MEMORY_MB));
        assert!(!valid_memory_mb(0.0));
        assert!(!valid_memory_mb(-64.0));
        assert!(!valid_memory_mb(0.5));
        assert!(!valid_memory_mb(64.25));
        assert!(!valid_memory_mb(MAX_MEMORY_MB + 1.0));
        assert!(!valid_memory_mb(f64::NAN));
        assert!(!valid_memory_mb(f64::INFINITY));
    }

    #[test]
    fn cpu_limits_are_positive_and_within_a_year() {
        assert!(valid_cpu_seconds(0.5));
        assert!(valid_cpu_seconds(3600.0));
        assert!(valid_cpu_seconds(MAX_CPU_SECONDS));
        assert!(!valid_cpu_seconds(0.0));
        assert!(!valid_cpu_seconds(-1.0));
        assert!(!valid_cpu_seconds(MAX_CPU_SECONDS * 2.0));
        assert!(!valid_cpu_seconds(f64::NAN));
    }

    #[test]
    fn no_limits_is_the_default() {
        assert!(Limits::default().is_none());
        assert!(!Limits { memory_mb: Some(64.0), cpu_seconds: None }.is_none());
        assert!(!Limits { memory_mb: None, cpu_seconds: Some(5.0) }.is_none());
    }

    /// Loads a sidecar list with one entry, `s`, whose JSON fields after `command` are `extra`.
    fn load_entry(name: &str, extra: &str) -> std::result::Result<crate::sidecar::SidecarConfig, String> {
        let path = std::env::temp_dir().join(format!("ironav-limits-{}-{name}.json", std::process::id()));
        std::fs::write(&path, format!(r#"{{"s": {{"command": ["program"]{extra}}}}}"#)).unwrap();
        let r = crate::sidecar::load(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        r.map(|mut m| m.remove("s").unwrap()).map_err(|e| format!("{e:?}"))
    }

    #[test]
    fn the_sidecar_file_sets_the_limits_and_unset_means_none() {
        let c = load_entry("set", r#", "max_memory_mb": 4096, "max_cpu_seconds": 120.5"#).unwrap();
        assert_eq!(c.limits(), Limits { memory_mb: Some(4096.0), cpu_seconds: Some(120.5) });
        let c = load_entry("unset", "").unwrap();
        assert!(c.limits().is_none());
    }

    #[test]
    fn the_sidecar_file_refuses_bad_limits_and_names_the_field() {
        let bad = [
            (r#", "max_memory_mb": 0"#, "max_memory_mb"),
            (r#", "max_memory_mb": -64"#, "max_memory_mb"),
            (r#", "max_memory_mb": 1.5"#, "max_memory_mb"),
            (r#", "max_memory_mb": 2000000"#, "max_memory_mb"),
            (r#", "max_cpu_seconds": 0"#, "max_cpu_seconds"),
            (r#", "max_cpu_seconds": -5"#, "max_cpu_seconds"),
            (r#", "max_cpu_seconds": 1e9"#, "max_cpu_seconds"),
        ];
        for (i, (extra, field)) in bad.iter().enumerate() {
            let e = load_entry(&format!("bad{i}"), extra).unwrap_err();
            assert!(e.contains(field), "{extra}: {e}");
        }
    }
}
