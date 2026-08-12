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
    #[cfg(not(target_os = "macos"))]
    mountpoint: CString,
    #[cfg(target_os = "macos")]
    channel_owner: Option<MountedChannelOwner>,
}

impl MountImpl {
    pub(crate) fn new(
        mountpoint: &Path,
        options: &[MountOption],
        acl: SessionACL,
    ) -> io::Result<(Channel, MountImpl)> {
        let mountpoint = CString::new(mountpoint.as_os_str().as_bytes()).unwrap();
        with_fuse_args(options, acl, |args| {
            #[cfg(target_os = "macos")]
            {
                let (channel, channel_owner) = MountedChannelOwner::mount(&mountpoint, args)?;
                Ok((
                    Channel::from_darwin(channel),
                    MountImpl {
                        channel_owner: Some(channel_owner),
                    },
                ))
            }

            #[cfg(not(target_os = "macos"))]
            {
                let fd = unsafe { fuse_mount_compat25(mountpoint.as_ptr(), args) };
                if fd < 0 {
                    Err(ensure_last_os_error())
                } else {
                    let file = unsafe { File::from_raw_fd(fd) };
                    Ok((
                        Channel::from_device(Arc::new(DevFuse(file))),
                        MountImpl { mountpoint },
                    ))
                }
            }
        })
    }

    pub(crate) fn umount_impl(&mut self) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            match self.channel_owner.take() {
                Some(owner) => owner.close(),
                None => Ok(()),
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
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
                        return Ok(());
                    }
                }
                return Err(error.into());
            }
            Ok(())
        }
    }
}
