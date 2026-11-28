//! RAII ownership of an acquired local advisory sidecar lock.
use std::fs::File;
use std::io;

#[derive(Debug)]
pub(super) struct SidecarLock(File);

impl SidecarLock {
    pub(super) fn acquire(file: File, contention: &'static str) -> io::Result<Self> {
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => {
                io::Error::new(io::ErrorKind::WouldBlock, contention)
            }
            std::fs::TryLockError::Error(error) => error,
        })?;
        Ok(Self(file))
    }

    #[cfg(test)]
    pub(super) fn duplicate_descriptor(&self) -> io::Result<File> {
        self.0.try_clone()
    }
}

impl Drop for SidecarLock {
    fn drop(&mut self) {
        // An inherited duplicate may outlive this logical owner. Never unlink
        // the stable sidecar, and never unlock a descriptor that failed to acquire.
        let _ = self.0.unlock();
    }
}
