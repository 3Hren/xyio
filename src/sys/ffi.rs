use libc::{c_void, c_int};

#[repr(C)]
pub struct IoVec {
    iov_base: *mut c_void,
    iov_len: c_int,
}
