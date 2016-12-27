use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

use nix::unistd::close;

/// Auto-closable raw file descriptor owner.
pub struct FileDesc {
    fd: RawFd,
}

impl Drop for FileDesc {
    fn drop(&mut self) {
        // Note that errors aren't handled when closing a file descriptor. The reason for this is
        // that if an error occurs we don't actually know if the file descriptor was closed or not,
        // and if we retried (for something like EINTR), we might close another valid file
        // descriptor opened after we closed ours.
        // Also note that this syscall may block, for example when there are unflushed buffers.
        if let Err(err) = close(self.fd) {
            error!("failed to close file descriptor {}: {:?}", self.fd, err);
        } else {
            debug!("closed fd {}", self.fd);
        }
    }
}

impl FromRawFd for FileDesc {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        FileDesc { fd: fd }
    }
}

impl AsRawFd for FileDesc {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}
