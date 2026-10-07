//! Sort cost estimate and the free-space check (spec §8.4, §10.4, §16).

use std::path::Path;

use super::{SortKey, key::RECORD_BYTES};
use crate::jobs::JobError;

/// Bytes per row of the permutation file.
pub const RESULT_BYTES: u64 = 8;

/// What a sort will cost (shown in the palette preview, M6-02).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortEstimate {
    /// Rows to sort.
    pub rows: u64,
    /// Disk needed in the temp dir: runs (24 B/row, external sorts only)
    /// plus the permutation file (8 B/row).
    pub disk_bytes: u64,
    /// Memory the sort buffers: `min(0.75 × free, needed)` (§10.4).
    pub ram_cap: u64,
    /// Everything fits in one buffer: no runs are written.
    pub in_memory: bool,
}

/// Estimates a sort of `rows` rows when `budget_free` bytes of the memory
/// budget are free. Every key layout uses 24-byte records (the first key is
/// encoded, later keys are read from the file on ties), so `keys` does not
/// change the estimate.
pub fn estimate(rows: u64, keys: &[SortKey], budget_free: u64) -> SortEstimate {
    let _ = keys;
    let needed = rows.saturating_mul(RECORD_BYTES as u64);
    let ram_cap = (budget_free / 4 * 3).min(needed);
    let in_memory = needed <= ram_cap;
    let result = rows.saturating_mul(RESULT_BYTES);
    let disk_bytes = if in_memory {
        result
    } else {
        needed.saturating_add(result)
    };
    SortEstimate {
        rows,
        disk_bytes,
        ram_cap,
        in_memory,
    }
}

impl SortEstimate {
    /// `est. 48.2 GB on disk → external merge sort, ~2.1 GB RAM cap`, or
    /// `est. 120 MB → in-memory sort` (the memory the in-memory sort needs).
    pub fn text(&self) -> String {
        if self.in_memory {
            format!("est. {} → in-memory sort", format_bytes(self.ram_cap))
        } else {
            format!(
                "est. {} on disk → external merge sort, ~{} RAM cap",
                format_bytes(self.disk_bytes),
                format_bytes(self.ram_cap)
            )
        }
    }
}

/// Decimal units with one decimal below 100: `48.2 GB`, `120 MB`, `512 B`.
pub fn format_bytes(n: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1_000_000_000_000, "TB"),
        (1_000_000_000, "GB"),
        (1_000_000, "MB"),
        (1_000, "KB"),
    ];
    for (scale, unit) in UNITS {
        if n >= scale {
            let tenths = (u128::from(n) * 10 / u128::from(scale)) as u64;
            return if tenths >= 1000 {
                format!("{} {unit}", tenths / 10)
            } else {
                format!("{}.{} {unit}", tenths / 10, tenths % 10)
            };
        }
    }
    format!("{n} B")
}

/// Free bytes for unprivileged users on the filesystem of `dir`
/// (`statvfs`). `None` when unknown (non-Unix, or the call failed).
pub fn free_space(dir: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
        // SAFETY: `path` is a valid NUL-terminated string and `st` a properly
        // sized out-parameter, initialised by `statvfs` on success.
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(path.as_ptr(), &mut st) };
        if rc != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

/// Refuses the sort when `dir` has less than `needed × 1.1` bytes free:
/// `not enough space in /tmp: need 48.2 GB, 12.0 GB free (use --tmp)`.
/// Skipped where free space is unknown (non-Unix).
pub fn check_space(dir: &Path, needed: u64) -> Result<(), JobError> {
    let Some(free) = free_space(dir) else {
        return Ok(());
    };
    let want = needed.saturating_add(needed / 10);
    if free < want {
        return Err(JobError::Other(format!(
            "not enough space in {}: need {}, {} free (use --tmp)",
            dir.display(),
            format_bytes(needed),
            format_bytes(free)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_and_in_memory() {
        let e = estimate(2_000_000_000, &[], 4 << 30);
        assert!(!e.in_memory);
        assert_eq!(e.disk_bytes, 2_000_000_000 * 32);
        assert_eq!(e.ram_cap, (4u64 << 30) / 4 * 3);
        assert_eq!(
            e.text(),
            "est. 64.0 GB on disk → external merge sort, ~3.2 GB RAM cap"
        );
        let e = estimate(5_000_000, &[], 2 << 30);
        assert!(e.in_memory);
        assert_eq!(e.ram_cap, 120_000_000);
        assert_eq!(e.disk_bytes, 40_000_000);
        assert_eq!(e.text(), "est. 120 MB → in-memory sort");
    }

    #[test]
    fn bytes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(48_200_000_000), "48.2 GB");
        assert_eq!(format_bytes(120_000_000), "120 MB");
        assert_eq!(format_bytes(1_500), "1.5 KB");
    }

    #[cfg(unix)]
    #[test]
    fn space_check() {
        let dir = std::env::temp_dir();
        assert!(free_space(&dir).is_some());
        assert!(check_space(&dir, 1).is_ok());
        let err = check_space(&dir, u64::MAX / 2).unwrap_err().to_string();
        assert!(err.starts_with("not enough space in "), "{err}");
        assert!(err.ends_with("free (use --tmp)"), "{err}");
    }
}
