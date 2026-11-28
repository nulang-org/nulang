//! Compaction admission estimates. This is not a filesystem reservation:
//! concurrent writers, quotas, metadata allocation and delayed allocation
//! can still produce ENOSPC. Existing recovery/fencing remains mandatory.
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionBudget {
    /// Maximum estimated additional allocated file bytes across volumes.
    /// Does not include existing files or filesystem metadata overhead.
    pub max_additional_bytes: u64,
    /// Minimum free-space headroom after estimated new file allocations,
    /// checked independently for each affected filesystem.
    pub min_free_bytes: u64,
}

impl Default for CompactionBudget {
    fn default() -> Self {
        Self {
            max_additional_bytes: u64::MAX,
            min_free_bytes: 0,
        }
    }
}

fn denied(reason: &str) -> io::Error {
    io::Error::other(format!("compaction storage admission: {reason}"))
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Demand<'a> {
    pub directory: &'a Path,
    pub bytes: u64,
    pub files: u64,
}

#[derive(Debug, Clone, Copy)]
struct Space {
    device: u64,
    block: u64,
    available: u64,
}

fn filesystem_space(path: &Path) -> io::Result<Space> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() {
        return Err(denied("not a directory"));
    }
    let cpath = CString::new(path.as_os_str().as_bytes()).map_err(|_| denied("invalid path"))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: cpath is a valid NUL-terminated pathname, and statvfs
    // initializes the writable struct on success.
    let status = unsafe { libc::statvfs(cpath.as_ptr(), stat.as_mut_ptr()) };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful statvfs call above initialized stat.
    let stat = unsafe { stat.assume_init() };
    let block = u64::try_from(i128::from(stat.f_frsize))
        .map_err(|_| denied("invalid filesystem fragment size"))?;
    if block == 0 {
        return Err(denied("zero filesystem fragment size"));
    }
    let available = u64::try_from(i128::from(stat.f_bavail))
        .map_err(|_| denied("invalid available block count"))?
        .checked_mul(block)
        .ok_or_else(|| denied("free-space overflow"))?;
    Ok(Space {
        device: metadata.dev(),
        block,
        available,
    })
}

fn check(
    budget: CompactionBudget,
    demands: impl IntoIterator<Item = (Space, u64, u64)>,
) -> io::Result<()> {
    let mut volumes = BTreeMap::<u64, (u64, u64)>::new();
    let mut total = 0u64;
    for (space, bytes, files) in demands {
        if space.block == 0 {
            return Err(denied("zero allocation unit"));
        }
        // Upper-bound file data rounding without needing each file length.
        let rounded = bytes
            .checked_add(
                files
                    .checked_mul(space.block - 1)
                    .ok_or_else(|| denied("allocation estimate overflow"))?,
            )
            .ok_or_else(|| denied("allocation estimate overflow"))?;
        total = total
            .checked_add(rounded)
            .ok_or_else(|| denied("aggregate estimate overflow"))?;
        let volume = volumes.entry(space.device).or_insert((0, space.available));
        volume.0 = volume
            .0
            .checked_add(rounded)
            .ok_or_else(|| denied("filesystem estimate overflow"))?;
        volume.1 = volume.1.min(space.available);
    }
    if total > budget.max_additional_bytes {
        return Err(denied("operation byte budget exceeded"));
    }
    for (_, (required, available)) in volumes {
        if required
            .checked_add(budget.min_free_bytes)
            .is_none_or(|n| n > available)
        {
            return Err(denied(
                "insufficient available filesystem space or headroom",
            ));
        }
    }
    Ok(())
}

pub(super) fn admit(budget: CompactionBudget, demands: &[Demand<'_>]) -> io::Result<()> {
    let demands = demands
        .iter()
        .map(|d| Ok((filesystem_space(d.directory)?, d.bytes, d.files)))
        .collect::<io::Result<Vec<_>>>()?;
    check(budget, demands)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_volume_demands_accumulate_and_separate_volumes_keep_headroom() {
        let a = Space {
            device: 1,
            block: 1,
            available: 100,
        };
        let b = Space {
            device: 2,
            block: 1,
            available: 100,
        };
        let budget = CompactionBudget {
            max_additional_bytes: 120,
            min_free_bytes: 10,
        };
        assert!(check(budget, [(a, 50, 1), (a, 50, 1)]).is_err());
        assert!(check(budget, [(a, 50, 1), (b, 50, 1)]).is_ok());
        assert!(check(budget, [(a, 65, 1), (b, 65, 1)]).is_err());
        assert!(check(budget, [(a, 91, 1)]).is_err());
        assert!(check(budget, [(a, 90, 1)]).is_ok());
        let lower = Space { available: 80, ..a };
        assert!(check(budget, [(a, 40, 1), (lower, 40, 1)]).is_err());
    }

    #[test]
    fn allocation_rounding_and_overflow_fail_closed() {
        let space = Space {
            device: 1,
            block: 4096,
            available: u64::MAX,
        };
        let budget = CompactionBudget {
            max_additional_bytes: 4096,
            min_free_bytes: 0,
        };
        assert!(check(budget, [(space, 1, 1)]).is_ok());
        assert!(check(budget, [(space, 2, 2)]).is_err());
        assert!(check(CompactionBudget::default(), [(space, u64::MAX, 1)]).is_err());
        assert!(check(
            CompactionBudget {
                min_free_bytes: u64::MAX,
                ..CompactionBudget::default()
            },
            [(space, 1, 1)]
        )
        .is_err());
    }
}
