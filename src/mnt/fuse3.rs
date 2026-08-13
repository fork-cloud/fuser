use std::ffi::CString;
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use std::time::Instant;

use crate::SessionACL;
use crate::dev_fuse::DevFuse;
use crate::mnt::MountOption;
use crate::mnt::fuse3_sys::fuse_lowlevel_ops;
use crate::mnt::fuse3_sys::fuse_session_destroy;
use crate::mnt::fuse3_sys::fuse_session_fd;
use crate::mnt::fuse3_sys::fuse_session_mount;
use crate::mnt::fuse3_sys::fuse_session_new;
use crate::mnt::fuse3_sys::fuse_session_unmount;
use crate::mnt::with_fuse_args;

/// Ensures that an os error is never 0/Success
fn ensure_last_os_error() -> io::Error {
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(0) => io::Error::new(io::ErrorKind::Other, "Unspecified Error"),
        _ => err,
    }
}

#[derive(Debug)]
pub(crate) struct MountImpl {
    fuse_session: Option<*mut c_void>,
    mountpoint: CString,
    mounted: bool,
}
impl MountImpl {
    pub(crate) fn prepare(mnt: &Path) -> io::Result<Self> {
        let mountpoint = CString::new(mnt.as_os_str().as_bytes()).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("mountpoint contains a null byte: {error}"),
            )
        })?;
        Ok(Self {
            fuse_session: None,
            mountpoint,
            mounted: false,
        })
    }

    pub(crate) fn mount(
        &mut self,
        options: &[MountOption],
        acl: SessionACL,
    ) -> io::Result<Arc<DevFuse>> {
        if self.fuse_session.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "libfuse3 mount owner was already used",
            ));
        }
        with_fuse_args(options, acl, |args| {
            let ops = fuse_lowlevel_ops::default();

            let fuse_session = unsafe {
                fuse_session_new(
                    args,
                    &ops as *const _,
                    size_of::<fuse_lowlevel_ops>(),
                    ptr::null_mut(),
                )
            };
            if fuse_session.is_null() {
                return Err(io::Error::last_os_error());
            }
            self.fuse_session = Some(fuse_session);
            let result = unsafe { fuse_session_mount(fuse_session, self.mountpoint.as_ptr()) };
            if result != 0 {
                return Err(ensure_last_os_error());
            }
            self.mounted = true;
            let fd = unsafe { fuse_session_fd(fuse_session) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = unsafe { BorrowedFd::borrow_raw(fd) };
            // We dup the fd here as the existing fd is owned by the fuse_session, and we
            // don't want it being closed out from under us:
            let fd = fd.try_clone_to_owned()?;
            let file = File::from(fd);
            Ok(Arc::new(DevFuse(file)))
        })
    }

    pub(crate) fn umount_impl(&mut self, _deadline: Instant) -> io::Result<()> {
        let Some(fuse_session) = self.fuse_session else {
            return Ok(());
        };
        if self.mounted {
            if let Err(err) = crate::mnt::libc_umount(&self.mountpoint) {
                // Linux always returns EPERM for non-root users.  We have to let the
                // library go through the setuid-root "fusermount -u" to unmount.
                if err == nix::errno::Errno::EPERM {
                    #[cfg(target_os = "linux")]
                    unsafe {
                        fuse_session_unmount(fuse_session);
                    }
                    #[cfg(target_os = "linux")]
                    {
                        self.mounted = false;
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        return Err(err.into());
                    }
                } else {
                    return Err(err.into());
                }
            } else {
                self.mounted = false;
            }
        }
        unsafe { fuse_session_destroy(fuse_session) };
        self.fuse_session = None;
        Ok(())
    }
}
unsafe impl Send for MountImpl {}
