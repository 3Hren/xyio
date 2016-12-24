use libc::{c_int, sockaddr, socklen_t};

extern "C" {
    /// Linux >= 2.6.27.
    pub fn epoll_create1(flags: c_int) -> c_int;

    /// Linux >= 2.6.28.
    pub fn accept4(fd: c_int, addr: *mut sockaddr, addrlen: *mut socklen_t, flags: c_int) -> c_int;
}
