//! FUSE request and reply communication
//!
//! Raw communication channels exposed by mounted FUSE providers.

#[cfg(fuser_mount_impl = "libfuse2")]
mod fuse2;
#[cfg(any(test, fuser_mount_impl = "libfuse2", fuser_mount_impl = "libfuse3"))]
pub(crate) mod fuse2_sys;
#[cfg(fuser_mount_impl = "libfuse3")]
mod fuse3;
#[cfg(fuser_mount_impl = "libfuse3")]
mod fuse3_sys;

#[cfg(fuser_mount_impl = "pure-rust")]
mod fuse_pure;
pub(crate) mod mount_options;

use std::io;

#[cfg(any(test, fuser_mount_impl = "libfuse2", fuser_mount_impl = "libfuse3"))]
use fuse2_sys::fuse_args;
use log::info;
use log::warn;
use mount_options::MountOption;

use crate::channel::Channel;

/// Helper function to provide options as a `fuse_args` struct
/// (which contains an argc count and an argv pointer)
#[cfg(any(test, fuser_mount_impl = "libfuse2", fuser_mount_impl = "libfuse3"))]
fn with_fuse_args<T, F: FnOnce(&mut fuse_args) -> T>(
    options: &[MountOption],
    acl: SessionACL,
    f: F,
) -> T {
    use std::ffi::CString;

    use mount_options::option_to_string;

    let mut args = vec![CString::new("rust-fuse").unwrap()];
    for x in options {
        args.extend_from_slice(&[
            CString::new("-o").unwrap(),
            CString::new(option_to_string(x)).unwrap(),
        ]);
    }
    if let Some(acl) = acl.to_mount_option() {
        args.push(CString::new("-o").unwrap());
        args.push(CString::new(acl).unwrap());
    }
    let argptrs: Vec<_> = args.iter().map(|s| s.as_ptr()).collect();
    f(&mut fuse_args {
        argc: argptrs.len() as i32,
        argv: argptrs.as_ptr(),
        allocated: 0,
    })
}

#[cfg(any(
    fuser_mount_impl = "pure-rust",
    fuser_mount_impl = "libfuse3",
    all(fuser_mount_impl = "libfuse2", not(target_os = "macos"))
))]
use std::ffi::CStr;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use crate::SessionACL;

#[derive(Debug)]
enum MountImpl {
    #[cfg(fuser_mount_impl = "pure-rust")]
    Pure(fuse_pure::MountImpl),
    #[cfg(fuser_mount_impl = "libfuse2")]
    Fuse2(fuse2::MountImpl),
    #[cfg(fuser_mount_impl = "libfuse3")]
    Fuse3(fuse3::MountImpl),
}

impl MountImpl {
    fn prepare(mountpoint: &Path) -> io::Result<Self> {
        #[cfg(fuser_mount_impl = "pure-rust")]
        {
            Ok(Self::Pure(fuse_pure::MountImpl::prepare(mountpoint)?))
        }
        #[cfg(fuser_mount_impl = "libfuse2")]
        {
            Ok(Self::Fuse2(fuse2::MountImpl::prepare(mountpoint)?))
        }
        #[cfg(fuser_mount_impl = "libfuse3")]
        {
            Ok(Self::Fuse3(fuse3::MountImpl::prepare(mountpoint)?))
        }
        #[cfg(fuser_mount_impl = "macos-no-mount")]
        {
            let _ = mountpoint;
            Err(io::Error::other(
                "Mount is not enabled; this is test-only configuration",
            ))
        }
    }

    fn mount(&mut self, options: &[MountOption], acl: SessionACL) -> io::Result<Channel> {
        match self {
            #[cfg(fuser_mount_impl = "pure-rust")]
            Self::Pure(mount) => mount.mount(options, acl).map(Channel::from_device),
            #[cfg(fuser_mount_impl = "libfuse2")]
            Self::Fuse2(mount) => mount.mount(options, acl),
            #[cfg(fuser_mount_impl = "libfuse3")]
            Self::Fuse3(mount) => mount.mount(options, acl).map(Channel::from_device),
            #[cfg(fuser_mount_impl = "macos-no-mount")]
            _ => {
                let _ = (options, acl);
                Err(io::Error::other(
                    "Mount is not enabled; this is test-only configuration",
                ))
            }
        }
    }

    fn umount_impl(&mut self, deadline: Instant) -> io::Result<()> {
        match self {
            #[cfg(fuser_mount_impl = "pure-rust")]
            MountImpl::Pure(mount) => mount.umount_impl(deadline),
            #[cfg(fuser_mount_impl = "libfuse2")]
            MountImpl::Fuse2(mount) => mount.umount_impl(deadline),
            #[cfg(fuser_mount_impl = "libfuse3")]
            MountImpl::Fuse3(mount) => mount.umount_impl(deadline),
            // This branch is needed because Rust does not consider & empty enum non-empty.
            #[cfg(fuser_mount_impl = "macos-no-mount")]
            _ => {
                let _ = deadline;
                Ok(())
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct Mount {
    mount_impl: Option<MountImpl>,
    mount_point: PathBuf,
}

fn release_retained<T>(
    owner: &mut Option<T>,
    release: impl FnOnce(&mut T) -> io::Result<()>,
) -> io::Result<()> {
    let Some(value) = owner.as_mut() else {
        return Ok(());
    };
    release(value)?;
    *owner = None;
    Ok(())
}

impl Mount {
    pub(crate) fn prepare(mountpoint: &Path) -> io::Result<Self> {
        Ok(Self {
            mount_impl: Some(MountImpl::prepare(mountpoint)?),
            mount_point: mountpoint.to_path_buf(),
        })
    }

    pub(crate) fn mount(
        &mut self,
        options: &[MountOption],
        acl: SessionACL,
    ) -> io::Result<Channel> {
        let Some(mount) = self.mount_impl.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "mount owner was already released",
            ));
        };
        info!("Mounting {}", self.mount_point.display());
        mount.mount(options, acl)
    }

    pub(crate) fn new(
        mountpoint: &Path,
        options: &[MountOption],
        acl: SessionACL,
    ) -> io::Result<(Channel, Self)> {
        let mut mount = Self::prepare(mountpoint)?;
        let channel = mount.mount(options, acl)?;
        Ok((channel, mount))
    }

    pub(crate) fn umount(&mut self, deadline: Instant) -> io::Result<()> {
        let mount_point = &self.mount_point;
        release_retained(&mut self.mount_impl, |mount| {
            info!("Unmounting {}", mount_point.display());
            mount.umount_impl(deadline)
        })
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        if let Err(err) = self.umount(deadline) {
            // This is not necessarily an error: may happen if a user called 'umount'.
            warn!("Unmount failed: {}", err);
        }
    }
}

#[cfg(any(
    fuser_mount_impl = "pure-rust",
    fuser_mount_impl = "libfuse3",
    all(fuser_mount_impl = "libfuse2", not(target_os = "macos"))
))]
fn libc_umount(mnt: &CStr) -> nix::Result<()> {
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        nix::mount::unmount(mnt, nix::mount::MntFlags::empty())
    }

    #[cfg(not(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    {
        nix::mount::umount(mnt)
    }
}

/// Warning: This will return true if the filesystem has been detached (lazy unmounted), but not
/// yet destroyed by the kernel.
#[cfg(any(all(not(target_os = "macos"), test), fuser_mount_impl = "pure-rust"))]
fn is_mounted(device: &impl std::os::fd::AsFd) -> bool {
    use std::slice;

    use nix::poll::PollFd;
    use nix::poll::PollFlags;
    use nix::poll::PollTimeout;
    use nix::poll::poll;

    loop {
        let mut poll_fd = PollFd::new(device.as_fd(), PollFlags::empty());
        let res = poll(slice::from_mut(&mut poll_fd), PollTimeout::ZERO);
        break match res {
            Ok(0) => true,
            Ok(1) => poll_fd
                .revents()
                .is_some_and(|r| r.contains(PollFlags::POLLERR)),
            Ok(_) => unreachable!(),
            Err(nix::errno::Errno::EINTR) => continue,
            Err(err) => {
                // This should never happen. The fd is guaranteed good as `File` owns it.
                // According to man poll ENOMEM is the only error code unhandled, so we panic
                // consistent with rust's usual ENOMEM behaviour.
                panic!("Poll failed with error {err}")
            }
        };
    }
}

#[cfg(test)]
mod test {
    use std::ffi::CStr;

    use crate::mnt::*;

    #[test]
    fn fuse_args() {
        with_fuse_args(
            &[
                MountOption::CUSTOM("foo".into()),
                MountOption::CUSTOM("bar".into()),
            ],
            SessionACL::RootAndOwner,
            |args| {
                let v: Vec<_> = (0..args.argc)
                    .map(|n| unsafe {
                        CStr::from_ptr(*args.argv.offset(n as isize))
                            .to_str()
                            .unwrap()
                    })
                    .collect();
                assert_eq!(
                    *v,
                    ["rust-fuse", "-o", "foo", "-o", "bar", "-o", "allow_other"]
                );
            },
        );
    }

    #[test]
    fn mac_fskit_mount_argument_is_owner_only() {
        with_fuse_args(&[MountOption::MacFsKit], SessionACL::Owner, |args| {
            let values: Vec<_> = (0..args.argc)
                .map(|index| unsafe {
                    CStr::from_ptr(*args.argv.offset(index as isize))
                        .to_str()
                        .unwrap()
                })
                .collect();
            assert_eq!(*values, ["rust-fuse", "-o", "backend=fskit"]);
        });
    }

    #[test]
    fn failed_unmount_retains_the_same_backend_for_retry() {
        #[derive(Debug, Eq, PartialEq)]
        struct Backend {
            attempts: usize,
        }

        let mut owner = Some(Backend { attempts: 0 });
        let first = release_retained(&mut owner, |backend| {
            backend.attempts += 1;
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "synthetic unmount failure",
            ))
        });
        assert_eq!(first.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(owner, Some(Backend { attempts: 1 }));

        release_retained(&mut owner, |backend| {
            backend.attempts += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(owner, None);
    }

    #[cfg(not(target_os = "macos"))]
    fn cmd_mount() -> String {
        std::str::from_utf8(
            std::process::Command::new("sh")
                .arg("-c")
                .arg("mount | grep fuse")
                .output()
                .unwrap()
                .stdout
                .as_ref(),
        )
        .unwrap()
        .to_owned()
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn mount_unmount() {
        use std::mem::ManuallyDrop;

        // We use ManuallyDrop here to leak the directory on test failure.  We don't
        // want to try and clean up the directory if it's a mountpoint otherwise we'll
        // deadlock.
        let tmp = ManuallyDrop::new(tempfile::tempdir().unwrap());
        let (channel, mount) = Mount::new(tmp.path(), &[], SessionACL::default()).unwrap();
        let mnt = cmd_mount();
        eprintln!("Our mountpoint: {:?}\nfuse mounts:\n{}", tmp.path(), mnt,);
        assert!(mnt.contains(&*tmp.path().to_string_lossy()));
        assert!(is_mounted(&channel));
        drop(mount);
        let mnt = cmd_mount();
        eprintln!("Our mountpoint: {:?}\nfuse mounts:\n{}", tmp.path(), mnt,);

        let detached = !mnt.contains(&*tmp.path().to_string_lossy());
        // Linux supports MNT_DETACH, so we expect unmount to succeed even if the FS
        // is busy.  Other systems don't so the unmount may fail and we will still
        // have the mount listed.  The mount will get cleaned up later.
        #[cfg(target_os = "linux")]
        assert!(detached);

        if detached {
            // We've detached successfully, it's safe to clean up:
            std::mem::ManuallyDrop::<_>::into_inner(tmp);
        }

        // Filesystem may have been lazy unmounted, so we can't assert this:
        // assert!(!is_mounted(&channel));
    }
}
