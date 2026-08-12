//! Native FFI bindings to libfuse2.
//!
//! This is a small set of bindings that are required to mount/unmount FUSE filesystems and
//! open/close a fd to the FUSE kernel driver.

#![warn(missing_debug_implementations)]
#![allow(missing_docs)]

use libc::c_char;
use libc::c_int;
#[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
use libc::c_void;
#[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
use libc::iovec;

#[repr(C)]
#[derive(Debug)]
pub(crate) struct fuse_args {
    pub argc: c_int,
    pub argv: *const *const c_char,
    pub allocated: c_int,
}

#[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
#[repr(C)]
#[derive(Debug)]
pub(crate) struct fuse_chan {
    _private: [u8; 0],
}

#[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
#[repr(C)]
#[derive(Debug)]
pub(crate) struct fuse_session {
    _private: [u8; 0],
}

#[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
type FuseSessionProcess = unsafe extern "C" fn(
    data: *mut c_void,
    buf: *const c_char,
    len: usize,
    channel: *mut fuse_chan,
);

#[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
type FuseSessionExit = unsafe extern "C" fn(data: *mut c_void, exited: c_int);

#[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
type FuseSessionExited = unsafe extern "C" fn(data: *mut c_void) -> c_int;

#[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
type FuseSessionDestroy = unsafe extern "C" fn(data: *mut c_void);

#[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
#[repr(C)]
#[derive(Debug)]
pub(crate) struct fuse_session_ops {
    pub process: Option<FuseSessionProcess>,
    pub exit: Option<FuseSessionExit>,
    pub exited: Option<FuseSessionExited>,
    pub destroy: Option<FuseSessionDestroy>,
}

#[cfg(fuser_mount_impl = "libfuse2")]
unsafe extern "C" {
    // *_compat25 functions were introduced in FUSE 2.6 when function signatures changed.
    // Therefore, the minimum version requirement for *_compat25 functions is libfuse-2.6.0.

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn fuse_mount_compat25(mountpoint: *const c_char, args: *const fuse_args) -> c_int;
    #[cfg(not(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    pub(crate) fn fuse_unmount_compat22(mountpoint: *const c_char);

    #[cfg(target_os = "macos")]
    pub(crate) fn fuse_mount(mountpoint: *const c_char, args: *mut fuse_args) -> *mut fuse_chan;
    #[cfg(target_os = "macos")]
    pub(crate) fn fuse_opt_free_args(args: *mut fuse_args);
    #[cfg(target_os = "macos")]
    pub(crate) fn fuse_session_new(
        ops: *mut fuse_session_ops,
        data: *mut c_void,
    ) -> *mut fuse_session;
    #[cfg(target_os = "macos")]
    pub(crate) fn fuse_session_add_chan(session: *mut fuse_session, channel: *mut fuse_chan);
    #[cfg(target_os = "macos")]
    pub(crate) fn fuse_session_exit(session: *mut fuse_session);
    #[cfg(target_os = "macos")]
    pub(crate) fn fuse_session_remove_chan(channel: *mut fuse_chan);
    #[cfg(target_os = "macos")]
    pub(crate) fn fuse_session_destroy(session: *mut fuse_session);
    #[cfg(target_os = "macos")]
    pub(crate) fn fuse_chan_recv(
        channel: *mut *mut fuse_chan,
        buffer: *mut c_char,
        size: usize,
    ) -> c_int;
    #[cfg(target_os = "macos")]
    pub(crate) fn fuse_chan_send(channel: *mut fuse_chan, iov: *const iovec, count: usize)
    -> c_int;
    #[cfg(target_os = "macos")]
    pub(crate) fn fuse_chan_destroy(channel: *mut fuse_chan);

    // These exact private exports are part of macFUSE 5.3.3's pinned ABI.
    #[cfg(target_os = "macos")]
    pub(crate) fn fuse_darwin_chan_interrupt(channel: *mut fuse_chan);
    #[cfg(target_os = "macos")]
    pub(crate) fn fuse_darwin_chan_unmount(channel: *mut fuse_chan);
}
