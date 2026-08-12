use std::io;
#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;
#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
use std::sync::Arc;

use nix::errno::Errno;

#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
use crate::dev_fuse::DevFuse;
use crate::passthrough::BackingId;

#[cfg(any(test, all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
pub(crate) mod darwin;

/// A raw FUSE request and reply channel.
#[derive(Debug, Clone)]
pub(crate) struct Channel(ChannelInner);

#[derive(Debug, Clone)]
enum ChannelInner {
    #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
    Device(Arc<DevFuse>),
    #[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
    Darwin(darwin::MountedChannel),
}

#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
impl AsFd for Channel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match &self.0 {
            #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
            ChannelInner::Device(device) => device.as_fd(),
        }
    }
}

impl Channel {
    #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
    pub(crate) fn from_device(device: Arc<DevFuse>) -> Self {
        Self(ChannelInner::Device(device))
    }

    #[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
    pub(crate) fn from_darwin(channel: darwin::MountedChannel) -> Self {
        Self(ChannelInner::Darwin(channel))
    }

    /// Receives data up to the capacity of the given buffer (can block).
    fn receive(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        match &self.0 {
            #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
            ChannelInner::Device(device) => nix::unistd::read(device, buffer),
            #[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
            ChannelInner::Darwin(channel) => channel.receive(buffer),
        }
    }

    /// Receives data up to the capacity of the given buffer (can block),
    /// retrying on errors that are safe to retry (ENOENT, EINTR, EAGAIN).
    ///
    /// - ENOENT: Operation interrupted. According to FUSE, this is safe to retry.
    /// - EINTR: Interrupted system call, retry.
    /// - EAGAIN: Explicitly instructed to try again.
    pub(crate) fn receive_retrying(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        loop {
            match self.receive(buffer) {
                Ok(size) => return Ok(size),
                Err(Errno::ENOENT | Errno::EINTR | Errno::EAGAIN) => continue,
                Err(err) => return Err(err),
            }
        }
    }

    /// Returns a sender object for this channel. The sender object can be
    /// used to send to the channel. Multiple sender objects can be used
    /// and they can safely be sent to other threads.
    pub(crate) fn sender(&self) -> ChannelSender {
        let inner = match &self.0 {
            // Since write/writev syscalls are threadsafe, senders can share the file.
            #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
            ChannelInner::Device(device) => ChannelSenderInner::Device(device.clone()),
            #[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
            ChannelInner::Darwin(channel) => ChannelSenderInner::Darwin(channel.sender()),
        };
        ChannelSender(inner)
    }

    /// Clone the FUSE device fd using FUSE_DEV_IOC_CLONE ioctl.
    ///
    /// This creates a new fd that can read FUSE requests independently,
    /// enabling true parallel request processing. The kernel distributes
    /// requests across all cloned fds.
    ///
    /// Requires Linux 4.5+. Returns an error on older kernels or non-Linux.
    #[cfg(target_os = "linux")]
    pub(crate) fn clone_fd(&self) -> io::Result<Channel> {
        use std::os::fd::AsRawFd;

        let new_dev = DevFuse::open()?;

        let ChannelInner::Device(device) = &self.0;
        let mut source_fd = device.as_raw_fd() as u32;
        // SAFETY: fuse_dev_ioc_clone is a valid ioctl for /dev/fuse
        unsafe {
            crate::ll::ioctl::fuse_dev_ioc_clone(new_dev.as_raw_fd(), &mut source_fd)?;
        }

        Ok(Channel::from_device(Arc::new(new_dev)))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ChannelSender(ChannelSenderInner);

#[derive(Clone, Debug)]
enum ChannelSenderInner {
    #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
    Device(Arc<DevFuse>),
    #[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
    Darwin(darwin::MountedSender),
}

impl ChannelSender {
    pub(crate) fn send(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        match &self.0 {
            #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
            ChannelSenderInner::Device(device) => {
                let rc = nix::sys::uio::writev(device, bufs)?;
                // writev is atomic, so do not need to check how many bytes are written.
                debug_assert_eq!(bufs.iter().map(|b| b.len()).sum::<usize>(), rc);
                Ok(())
            }
            #[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
            ChannelSenderInner::Darwin(sender) => sender.send(bufs),
        }
    }

    pub(crate) fn open_backing(&self, fd: BorrowedFd<'_>) -> std::io::Result<BackingId> {
        match &self.0 {
            #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
            ChannelSenderInner::Device(device) => BackingId::create(device, fd),
            #[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
            ChannelSenderInner::Darwin(_) => {
                let _ = fd;
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "backing IDs are unavailable on macFUSE's FSKit channel",
                ))
            }
        }
    }

    pub(crate) unsafe fn wrap_backing(&self, id: u32) -> BackingId {
        match &self.0 {
            #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
            ChannelSenderInner::Device(device) => unsafe { BackingId::wrap_raw(device, id) },
            #[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
            ChannelSenderInner::Darwin(_) => {
                let _ = id;
                panic!("backing IDs are unavailable on macFUSE's FSKit channel")
            }
        }
    }
}
