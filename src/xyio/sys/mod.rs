use std::ptr;

use libc;
use nix::errno::Errno;

mod ffi;

bitflags! {
    pub flags EPollFlags: i32 {
        const EPOLL_CLOEXEC = 0x80000,
    }
}

pub fn epoll_create(flags: EPollFlags) -> Result<i32, Errno> {
    let fd = unsafe {
        ffi::epoll_create1(flags.bits())
    };

    if fd >= 0 {
        Ok(fd)
    } else {
        Err(Errno::last())
    }
}

pub fn accept4(sockfd: libc::c_int, flags: libc::c_int) -> Result<i32, Errno> {
    let fd = unsafe {
        ffi::accept4(sockfd, ptr::null_mut(), ptr::null_mut(), flags)
    };

    if fd >= 0 {
        Ok(fd)
    } else {
        Err(Errno::last())
    }
}
