use std::io::Error;
use std::os::unix::io::RawFd;
use std::ptr;

use libc;
use nix::errno::Errno;
use nix::sys::socket;

mod ffi;

bitflags! {
    pub flags EPollFlags: i32 {
        const EPOLL_CLOEXEC = 0x80000,
    }
}

pub fn epoll_create(flags: EPollFlags) -> Result<i32, Errno> {
    let fd = unsafe {
        libc::epoll_create1(flags.bits())
    };

    if fd >= 0 {
        Ok(fd)
    } else {
        Err(Errno::last())
    }
}

pub fn accept4(sockfd: libc::c_int, flags: libc::c_int) -> Result<i32, Errno> {
    let fd = unsafe {
        libc::accept4(sockfd, ptr::null_mut(), ptr::null_mut(), flags)
    };

    if fd >= 0 {
        Ok(fd)
    } else {
        Err(Errno::last())
    }
}

pub fn sendmsg(fd: RawFd, iov: &[&[u8]], flags: socket::MsgFlags) -> Result<usize, Error> {
    let mhdr = libc::msghdr {
        msg_name: 0 as *mut libc::c_void,
        msg_namelen: 0,
        msg_iov: iov.as_ptr() as *mut libc::iovec,
        msg_iovlen: iov.len(),
        msg_control: 0 as *mut libc::c_void,
        msg_controllen: 0,
        msg_flags: 0,
    };

    let rc = unsafe {
        libc::sendmsg(fd, &mhdr, flags.bits())
    };

    if rc < 0 {
        Err(Error::last_os_error())
    } else {
        Ok(rc as usize)
    }
}
