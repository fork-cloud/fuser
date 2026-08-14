use std::io;
#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
use std::sync::Arc;
#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
use std::sync::Weak;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};

use nix::errno::Errno;
#[cfg(target_os = "linux")]
use nix::fcntl::{FcntlArg, OFlag, fcntl};
#[cfg(target_os = "linux")]
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
#[cfg(target_os = "linux")]
use parking_lot::Mutex;

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
    Device(DeviceChannel),
    #[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
    Darwin(darwin::MountedChannel),
}

#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
impl AsFd for Channel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match &self.0 {
            #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
            ChannelInner::Device(channel) => channel.device.as_fd(),
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct ReceiveInterrupt {
    stopped: AtomicBool,
    reader: OwnedFd,
    writer: Mutex<Option<OwnedFd>>,
}

#[cfg(target_os = "linux")]
impl ReceiveInterrupt {
    fn new() -> io::Result<Self> {
        let (reader, writer) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
        Ok(Self {
            stopped: AtomicBool::new(false),
            reader,
            writer: Mutex::new(Some(writer)),
        })
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.writer.lock().take();
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone)]
#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
struct DeviceChannel {
    device: Arc<DevFuse>,
    #[cfg(target_os = "linux")]
    interrupt: Arc<ReceiveInterrupt>,
}

#[derive(Debug, Clone)]
#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
struct DeviceSender {
    device: Weak<DevFuse>,
    #[cfg(target_os = "linux")]
    interrupt: Arc<ReceiveInterrupt>,
}

#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
impl DeviceChannel {
    fn new(device: Arc<DevFuse>) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            set_nonblocking(&device)?;
            return Ok(Self {
                device,
                interrupt: Arc::new(ReceiveInterrupt::new()?),
            });
        }
        #[cfg(not(target_os = "linux"))]
        Ok(Self { device })
    }

    #[cfg(target_os = "linux")]
    fn with_interrupt(device: Arc<DevFuse>, interrupt: Arc<ReceiveInterrupt>) -> io::Result<Self> {
        set_nonblocking(&device)?;
        Ok(Self { device, interrupt })
    }

    fn receive(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        #[cfg(target_os = "linux")]
        {
            loop {
                if self.interrupt.is_stopped() {
                    return Err(Errno::ENODEV);
                }
                match nix::unistd::read(&self.device, buffer) {
                    Ok(size) => return Ok(size),
                    Err(Errno::EAGAIN) => {}
                    Err(error) => return Err(error),
                }

                let mut descriptors = [
                    PollFd::new(self.device.as_fd(), PollFlags::POLLIN),
                    PollFd::new(self.interrupt.reader.as_fd(), PollFlags::POLLIN),
                ];
                poll(&mut descriptors, PollTimeout::NONE)?;
            }
        }
        #[cfg(not(target_os = "linux"))]
        nix::unistd::read(&self.device, buffer)
    }

    fn sender(&self) -> DeviceSender {
        DeviceSender {
            device: Arc::downgrade(&self.device),
            #[cfg(target_os = "linux")]
            interrupt: Arc::clone(&self.interrupt),
        }
    }
}

#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
impl DeviceSender {
    fn device(&self) -> io::Result<Arc<DevFuse>> {
        self.device.upgrade().ok_or_else(disconnected_error)
    }

    fn interrupt_receive(&self) {
        #[cfg(target_os = "linux")]
        {
            self.interrupt.stop();
        }
    }
}

#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
fn disconnected_error() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "FUSE channel is closed")
}

#[cfg(target_os = "linux")]
fn set_nonblocking(device: &Arc<DevFuse>) -> io::Result<()> {
    let flags = OFlag::from_bits_truncate(fcntl(device, FcntlArg::F_GETFL)?);
    fcntl(device, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok(())
}

impl Channel {
    #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
    pub(crate) fn from_device(device: Arc<DevFuse>) -> io::Result<Self> {
        DeviceChannel::new(device)
            .map(ChannelInner::Device)
            .map(Self)
    }

    #[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
    pub(crate) fn from_darwin(channel: darwin::MountedChannel) -> Self {
        Self(ChannelInner::Darwin(channel))
    }

    /// Receives data up to the capacity of the given buffer (can block).
    fn receive(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        match &self.0 {
            #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
            ChannelInner::Device(channel) => channel.receive(buffer),
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
            ChannelInner::Device(channel) => ChannelSenderInner::Device(channel.sender()),
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

        let ChannelInner::Device(channel) = &self.0;
        let mut source_fd = channel.device.as_raw_fd() as u32;
        // SAFETY: fuse_dev_ioc_clone is a valid ioctl for /dev/fuse
        unsafe {
            crate::ll::ioctl::fuse_dev_ioc_clone(new_dev.as_raw_fd(), &mut source_fd)?;
        }

        DeviceChannel::with_interrupt(Arc::new(new_dev), Arc::clone(&channel.interrupt))
            .map(ChannelInner::Device)
            .map(Channel)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ChannelSender(ChannelSenderInner);

#[derive(Clone, Debug)]
enum ChannelSenderInner {
    #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
    Device(DeviceSender),
    #[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
    Darwin(darwin::MountedSender),
}

impl ChannelSender {
    pub(crate) fn send(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        match &self.0 {
            #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
            ChannelSenderInner::Device(sender) => {
                let device = sender.device()?;
                let rc = nix::sys::uio::writev(&device, bufs)?;
                // writev is atomic, so do not need to check how many bytes are written.
                debug_assert_eq!(bufs.iter().map(|b| b.len()).sum::<usize>(), rc);
                Ok(())
            }
            #[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
            ChannelSenderInner::Darwin(sender) => sender.send(bufs),
        }
    }

    pub(crate) fn interrupt_receive(&self) {
        match &self.0 {
            #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
            ChannelSenderInner::Device(sender) => sender.interrupt_receive(),
            #[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
            ChannelSenderInner::Darwin(_) => {}
        }
    }

    pub(crate) fn open_backing(&self, fd: BorrowedFd<'_>) -> std::io::Result<BackingId> {
        match &self.0 {
            #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
            ChannelSenderInner::Device(sender) => BackingId::create(&sender.device()?, fd),
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
            ChannelSenderInner::Device(sender) => unsafe {
                BackingId::wrap_raw(sender.device.clone(), id)
            },
            #[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
            ChannelSenderInner::Darwin(_) => {
                let _ = id;
                panic!("backing IDs are unavailable on macFUSE's FSKit channel")
            }
        }
    }
}

#[cfg(all(test, not(all(target_os = "macos", fuser_mount_impl = "libfuse2"))))]
mod tests {
    use super::*;

    #[test]
    fn sender_does_not_retain_a_closed_device() {
        let (_reader, writer) = nix::unistd::pipe().unwrap();
        let channel = Channel::from_device(Arc::new(DevFuse(std::fs::File::from(writer)))).unwrap();
        let sender = channel.sender();
        drop(channel);

        let error = sender
            .send(&[])
            .expect_err("sender must not keep the device open");

        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
    }

    #[test]
    fn closed_sender_wraps_an_inert_backing_id() {
        let (_reader, writer) = nix::unistd::pipe().unwrap();
        let channel = Channel::from_device(Arc::new(DevFuse(std::fs::File::from(writer)))).unwrap();
        let sender = channel.sender();
        drop(channel);

        let backing = unsafe { sender.wrap_backing(7) };

        assert!(backing.channel.upgrade().is_none());
        assert_eq!(backing.backing_id, 7);
    }
}
