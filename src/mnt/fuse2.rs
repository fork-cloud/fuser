use std::ffi::CString;
#[cfg(not(target_os = "macos"))]
use std::fs::File;
use std::io;
#[cfg(not(target_os = "macos"))]
use std::os::unix::prelude::FromRawFd;
use std::os::unix::prelude::OsStrExt;
use std::path::Path;
#[cfg(not(target_os = "macos"))]
use std::sync::Arc;
use std::time::Instant;

use crate::SessionACL;
use crate::channel::Channel;
#[cfg(target_os = "macos")]
use crate::channel::darwin::MountedChannelOwner;
#[cfg(not(target_os = "macos"))]
use crate::dev_fuse::DevFuse;
use crate::mnt::MountOption;
#[cfg(not(target_os = "macos"))]
use crate::mnt::fuse2_sys::*;
use crate::mnt::with_fuse_args;

#[cfg(not(target_os = "macos"))]
fn ensure_last_os_error() -> io::Error {
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(0) => io::Error::other("libfuse returned no error detail"),
        _ => error,
    }
}

#[derive(Debug)]
pub(crate) struct MountImpl {
    mountpoint: CString,
    #[cfg(target_os = "macos")]
    channel_owner: MountedChannelOwner,
    #[cfg(not(target_os = "macos"))]
    mounted: bool,
}

impl MountImpl {
    pub(crate) fn prepare(mountpoint: &Path) -> io::Result<Self> {
        let mountpoint = CString::new(mountpoint.as_os_str().as_bytes()).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("mountpoint contains a null byte: {error}"),
            )
        })?;
        Ok(Self {
            mountpoint,
            #[cfg(target_os = "macos")]
            channel_owner: MountedChannelOwner::new(),
            #[cfg(not(target_os = "macos"))]
            mounted: false,
        })
    }

    pub(crate) fn mount(
        &mut self,
        options: &[MountOption],
        acl: SessionACL,
    ) -> io::Result<Channel> {
        with_fuse_args(options, acl, |args| {
            #[cfg(target_os = "macos")]
            {
                self.channel_owner
                    .mount(&self.mountpoint, args)
                    .map(Channel::from_darwin)
            }

            #[cfg(not(target_os = "macos"))]
            {
                if self.mounted {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "libfuse2 mount owner is already mounted",
                    ));
                }
                let fd = unsafe { fuse_mount_compat25(self.mountpoint.as_ptr(), args) };
                if fd < 0 {
                    Err(ensure_last_os_error())
                } else {
                    self.mounted = true;
                    let file = unsafe { File::from_raw_fd(fd) };
                    Ok(Channel::from_device(Arc::new(DevFuse(file))))
                }
            }
        })
    }

    pub(crate) fn umount_impl(&mut self, deadline: Instant) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            self.channel_owner.close(deadline)
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = deadline;
            if !self.mounted {
                return Ok(());
            }
            // Calling unmount directly avoids fuse_unmount_compat22's realpath lookup.
            if let Err(error) = crate::mnt::libc_umount(&self.mountpoint) {
                if error == nix::errno::Errno::EPERM {
                    #[cfg(not(any(
                        target_os = "freebsd",
                        target_os = "dragonfly",
                        target_os = "openbsd",
                        target_os = "netbsd"
                    )))]
                    unsafe {
                        fuse_unmount_compat22(self.mountpoint.as_ptr());
                        self.mounted = false;
                        return Ok(());
                    }
                }
                return Err(error.into());
            }
            self.mounted = false;
            Ok(())
        }
    }
}
