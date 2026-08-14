//! Filesystem session
//!
//! A session runs a filesystem implementation while it is being mounted to a specific mount
//! point. A session begins by mounting the filesystem and ends by unmounting it. While the
//! filesystem is mounted, the session loop receives, dispatches and replies to kernel requests
//! for filesystem operations under its mount point.

use std::borrow::Cow;
#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
use std::fs::File;
use std::io;
#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
use std::os::fd::AsFd;
#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
use std::os::fd::BorrowedFd;
#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::thread::{self};
use std::time::Duration;
use std::time::Instant;

use log::debug;
use log::error;
use log::warn;
use nix::unistd::Uid;
use nix::unistd::geteuid;
use parking_lot::Mutex;

use crate::Errno;
use crate::Filesystem;
use crate::KernelConfig;
use crate::MountOption;
use crate::ReplyEmpty;
use crate::Request;
use crate::channel::Channel;
use crate::channel::ChannelSender;
#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
use crate::dev_fuse::DevFuse;
use crate::ll;
use crate::ll::Operation;
use crate::ll::ResponseErrno;
use crate::ll::Version;
use crate::ll::flags::init_flags::InitFlags;
use crate::ll::fuse_abi as abi;
use crate::mnt::Mount;
use crate::mnt::mount_options::AclRequestIdentity;
use crate::mnt::mount_options::Config;
use crate::mnt::mount_options::validate_config;
use crate::notify::Notifier;
use crate::read_buf::FuseReadBuf;
use crate::reply::Reply;
use crate::reply::ReplyRaw;
use crate::reply::ReplySender;
use crate::request::RequestWithSender;

/// The max size of write requests from the kernel. The absolute minimum is 4k,
/// FUSE recommends at least 128k, max 16M. The FUSE default is 16M on macOS
/// and 128k on other systems.
pub(crate) const MAX_WRITE_SIZE: usize = 16 * 1024 * 1024;

#[derive(Default, Debug, Eq, PartialEq, Clone, Copy)]
/// How requests should be filtered based on the calling UID.
pub enum SessionACL {
    /// Allow requests from any user. Corresponds to the `allow_other` mount option.
    All,
    /// Allow requests from root. Corresponds to the `allow_root` mount option.
    RootAndOwner,
    /// Allow requests from the owning UID. This is FUSE's default mode of operation.
    #[default]
    Owner,
}

impl SessionACL {
    /// Returns the mount option string for kernel/fusermount/libfuse paths.
    /// Both `All` and `RootAndOwner` map to `allow_other` - the kernel only
    /// understands `allow_other`, and fuser enforces the root-only restriction internally.
    #[allow(dead_code)]
    pub(crate) fn to_mount_option(self) -> Option<&'static str> {
        match self {
            SessionACL::All | SessionACL::RootAndOwner => Some("allow_other"),
            SessionACL::Owner => None,
        }
    }
}

/// Calls `destroy` on drop.
#[derive(Debug)]
pub(crate) struct FilesystemHolder<FS: Filesystem> {
    pub(crate) fs: Option<FS>,
}

impl<FS: Filesystem> FilesystemHolder<FS> {
    fn destroy(&mut self) {
        if let Some(mut fs) = self.fs.take() {
            fs.destroy();
        }
    }
}

impl<FS: Filesystem> Drop for FilesystemHolder<FS> {
    fn drop(&mut self) {
        self.destroy();
    }
}

#[derive(Debug)]
struct UmountOnDrop {
    mount: Option<Mount>,
}

impl UmountOnDrop {
    fn umount(&mut self) -> io::Result<()> {
        self.umount_until(Instant::now() + Duration::from_secs(2))
    }

    fn umount_until(&mut self, deadline: Instant) -> io::Result<()> {
        let Some(owner) = self.mount.as_mut() else {
            return Ok(());
        };
        owner.umount(deadline)?;
        self.mount = None;
        Ok(())
    }

    fn empty() -> Self {
        Self { mount: None }
    }
}

impl Drop for UmountOnDrop {
    fn drop(&mut self) {
        if let Err(e) = self.umount() {
            warn!("Failed to umount filesystem: {}", e);
        }
    }
}

fn validate_mounted_session_config(options: &Config) -> io::Result<AclRequestIdentity> {
    let acl_request_identity = validate_config(options)?;
    if options.mount_options.contains(&MountOption::AutoUnmount) && options.acl == SessionACL::Owner
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("auto_unmount requires acl != Owner, got: {:?}", options.acl),
        ));
    }
    Ok(acl_request_identity)
}

/// The session data structure
#[derive(Debug)]
pub struct Session<FS: Filesystem> {
    /// Filesystem operation implementations. None after `destroy` called.
    pub(crate) filesystem: FilesystemHolder<FS>,
    /// Communication channel to the kernel driver
    pub(crate) ch: Channel,
    /// Handle to the mount.  Dropping this unmounts.
    mount: UmountOnDrop,
    /// Whether to restrict access to owner, root + owner, or unrestricted
    /// Used to implement `allow_root` and `auto_unmount`
    pub(crate) allowed: SessionACL,
    /// User that launched the fuser process
    pub(crate) session_owner: Uid,
    /// How provider credentials are interpreted for owner-only ACL comparison.
    pub(crate) acl_request_identity: AclRequestIdentity,
    /// FUSE protocol version, as reported by the kernel.
    /// The field is set to `Some` when the init message is received.
    pub(crate) proto_version: Option<Version>,
    pub(crate) config: Config,
}

#[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
impl<FS: Filesystem> AsFd for Session<FS> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.ch.as_fd()
    }
}

impl<FS: Filesystem> Session<FS> {
    /// Create a new session by mounting the given filesystem to the given mountpoint
    /// # Errors
    /// Returns an error if the options are incorrect, or if the fuse device can't be mounted.
    pub fn new<P: AsRef<Path>>(
        filesystem: FS,
        mountpoint: P,
        options: &Config,
    ) -> io::Result<Session<FS>> {
        let acl_request_identity = validate_mounted_session_config(options)?;

        let mountpoint = mountpoint.as_ref();
        let (ch, mount) = Mount::new(mountpoint, &options.mount_options, options.acl)?;

        let mut session = Session {
            filesystem: FilesystemHolder {
                fs: Some(filesystem),
            },
            ch,
            mount: UmountOnDrop { mount: Some(mount) },
            allowed: options.acl,
            session_owner: geteuid(),
            acl_request_identity,
            proto_version: None,
            config: options.clone(),
        };

        session.handshake()?;

        Ok(session)
    }

    /// Wrap an existing /dev/fuse file descriptor. This doesn't mount the
    /// filesystem anywhere; that must be done separately.
    #[cfg(not(all(target_os = "macos", fuser_mount_impl = "libfuse2")))]
    pub fn from_fd(
        filesystem: FS,
        fd: OwnedFd,
        acl: SessionACL,
        config: Config,
    ) -> io::Result<Self> {
        let acl_request_identity = validate_config(&config)?;
        let ch = Channel::from_device(Arc::new(DevFuse(File::from(fd))))?;
        let mut session = Session {
            filesystem: FilesystemHolder {
                fs: Some(filesystem),
            },
            ch,
            mount: UmountOnDrop::empty(),
            allowed: acl,
            session_owner: geteuid(),
            acl_request_identity,
            proto_version: None,
            config,
        };

        session.handshake()?;

        Ok(session)
    }

    /// Run the session loop in a background thread. If the returned handle is dropped,
    /// the filesystem is unmounted and the given session ends.
    pub fn spawn(mut self) -> io::Result<BackgroundSession>
    where
        FS: Send + 'static,
    {
        let sender = self.ch.sender();
        let mount = std::mem::replace(&mut self.mount, UmountOnDrop::empty());
        let session = Arc::new(Mutex::new(Some(self)));
        let (activation, activation_receiver) = mpsc::channel();
        let guard = thread::Builder::new()
            .name("fuser-bg".to_string())
            .spawn(move || run_after_activation(session, activation_receiver))?;
        let mut background = BackgroundSession {
            sender,
            mount,
            dispatch: DispatchOwner::new(guard, activation),
        };
        background.activate()?;
        Ok(background)
    }

    /// Run the session loop that receives kernel requests and dispatches them to method
    /// calls into the filesystem. This read-dispatch-loop is non-concurrent to prevent
    /// having multiple buffers (which take up much memory), but the filesystem methods
    /// may run concurrent by spawning threads.
    /// # Errors
    /// Returns any final error when the session comes to an end.
    pub fn run(self) -> io::Result<()> {
        let Session {
            filesystem,
            ch,
            mount: _do_not_umount_yet,
            allowed,
            session_owner,
            acl_request_identity,
            proto_version: _,
            config,
        } = self;

        let n_threads = config.n_threads.unwrap_or(1);

        if !cfg!(target_os = "linux") && n_threads != 1 {
            // TODO: check whether it works on macOS/FreeBSD and enable if it works.
            return Err(io::Error::other(
                "n_threads != 1 is only supported on Linux",
            ));
        }

        let Some(n_threads_minus_one) = n_threads.checked_sub(1) else {
            return Err(io::Error::other("n_threads"));
        };

        let mut filesystem = Arc::new(filesystem);

        let mut channels = Vec::with_capacity(n_threads);

        for _ in 0..n_threads_minus_one {
            if config.clone_fd {
                #[cfg(target_os = "linux")]
                {
                    channels.push(ch.clone_fd()?);
                    continue;
                }
                #[cfg(not(target_os = "linux"))]
                {
                    return Err(io::Error::other("clone_fd is only supported on Linux"));
                }
            } else {
                channels.push(ch.clone());
            }
        }
        channels.push(ch);

        let mut threads = Vec::with_capacity(n_threads);

        for (i, ch) in channels.into_iter().enumerate() {
            let thread_name = format!("fuser-{i}");
            let event_loop = SessionEventLoop {
                thread_name: thread_name.clone(),
                filesystem: filesystem.clone(),
                ch,
                allowed,
                session_owner,
                acl_request_identity,
            };
            threads.push(
                thread::Builder::new()
                    .name(thread_name)
                    .spawn(move || event_loop.event_loop())?,
            );
        }

        let mut reply: io::Result<()> = Ok(());
        for thread in threads {
            let res = match thread.join() {
                Ok(res) => res,
                Err(_) => {
                    return Err(io::Error::other("event loop thread panicked"));
                }
            };
            if let Err(e) = res {
                if reply.is_ok() {
                    reply = Err(e);
                }
            }
        }

        let Some(filesystem) = Arc::get_mut(&mut filesystem) else {
            return Err(io::Error::other(
                "BUG: must have one refcount for filesystem",
            ));
        };

        filesystem.destroy();

        reply
    }

    fn handshake(&mut self) -> io::Result<()> {
        let mut buf = FuseReadBuf::new();
        let buf = buf.as_mut();

        loop {
            // Read the init request from the kernel
            let size = match self.ch.receive_retrying(buf) {
                Ok(size) => size,
                Err(nix::errno::Errno::ENODEV) => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "FUSE device disconnected during handshake",
                    ));
                }
                Err(err) => return Err(err.into()),
            };

            // Parse the request
            let request = match ll::AnyRequest::try_from(&buf[..size]) {
                Ok(request) => request,
                Err(err) => {
                    error!("{err}");
                    return Err(io::Error::new(io::ErrorKind::InvalidData, err.to_string()));
                }
            };

            // Extract the init operation
            let op = match request.operation() {
                Ok(op) => op,
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Failed to parse FUSE operation",
                    ));
                }
            };

            let init = match op {
                ll::Operation::Init(init) => init,
                _ => {
                    error!("Received non-init FUSE operation before init: {}", request);
                    // Send error response and return error - non-init during handshake is invalid
                    <ReplyRaw as Reply>::new(
                        request.unique(),
                        ReplySender::Channel(self.ch.sender()),
                    )
                    .send_ll(&ResponseErrno(ll::Errno::EIO));
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Received non-init FUSE operation during handshake",
                    ));
                }
            };

            let v = init.version();
            if v.0 > abi::FUSE_KERNEL_VERSION {
                // Kernel has a newer major version than we support.
                // Send our version and wait for a second INIT request with a compatible version.
                debug!(
                    "INIT: Kernel version {} > our version {}, sending our version and waiting for next init",
                    v.0,
                    abi::FUSE_KERNEL_VERSION
                );
                let response = init.reply_version_only();
                <ReplyRaw as Reply>::new(request.unique(), ReplySender::Channel(self.ch.sender()))
                    .send_ll(&response);
                continue;
            }

            // We don't support ABI versions before 7.6
            if v < Version(7, 6) {
                error!("Unsupported FUSE ABI version {v}");
                <ReplyRaw as Reply>::new(request.unique(), ReplySender::Channel(self.ch.sender()))
                    .send_ll(&ResponseErrno(ll::Errno::EPROTO));
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("Unsupported FUSE ABI version {v}"),
                ));
            }

            let mut config = KernelConfig::new(init.capabilities(), init.max_readahead(), v);

            // Call filesystem init method and give it a chance to return an error
            let Some(filesystem) = &mut self.filesystem.fs else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Bug: filesystem must be initialized during handshake",
                ));
            };
            let res = filesystem.init(Request::ref_cast(request.header()), &mut config);
            if let Err(error) = res {
                let errno = Errno::from_i32(error.raw_os_error().unwrap_or(0));
                <ReplyRaw as Reply>::new(request.unique(), ReplySender::Channel(self.ch.sender()))
                    .send_ll(&ResponseErrno(errno));
                return Err(error);
            }

            // Remember the ABI version supported by kernel and mark the session initialized.
            self.proto_version = Some(v);

            // Log capability status for debugging
            for bit in 0..64 {
                let bitflags = InitFlags::from_bits_retain(1 << bit);
                #[cfg(not(target_os = "macos"))]
                if bitflags == InitFlags::FUSE_INIT_EXT {
                    continue;
                }
                let bitflag_is_known = InitFlags::all().contains(bitflags);
                let kernel_supports = init.capabilities().contains(bitflags);
                let we_requested = config.requested.contains(bitflags);
                let name = if let Some((name, _)) = bitflags.iter_names().next() {
                    Cow::Borrowed(name)
                } else {
                    Cow::Owned(format!("(1 << {bit})"))
                };
                if we_requested && kernel_supports {
                    debug!("capability {name} enabled")
                } else if we_requested {
                    debug!("capability {name} not supported by kernel")
                } else if kernel_supports {
                    debug!("capability {name} not requested by client")
                } else if bitflag_is_known {
                    debug!("capability {name} not supported nor requested")
                }
            }

            // Reply with our desired version and settings.
            debug!(
                "INIT response: ABI {}.{}, flags {:#x}, max readahead {}, max write {}",
                abi::FUSE_KERNEL_VERSION,
                abi::FUSE_KERNEL_MINOR_VERSION,
                init.capabilities() & config.requested,
                config.max_readahead,
                config.max_write
            );

            let response = init.reply(&config);
            <ReplyRaw as Reply>::new(request.unique(), ReplySender::Channel(self.ch.sender()))
                .send_ll(&response);

            return Ok(());
        }
    }

    /// Unmount the filesystem
    pub fn unmount(&mut self) -> io::Result<()> {
        self.mount.umount()
    }

    /// Returns an object that can be used to send notifications to the kernel
    pub fn notifier(&self) -> Notifier {
        Notifier::new(self.ch.sender())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionOwnerState {
    Prepared,
    Constructing,
    Transferred,
    Released,
}

/// Non-cloneable owner for recoverable native session construction.
///
/// [`SessionOwner::new`] performs validation and creates the owner before any
/// native mount call. [`SessionOwner::construct`] mutates it through a borrow,
/// so an error or caught unwind leaves the caller holding cleanup authority.
#[derive(Debug)]
pub struct SessionOwner<FS: Filesystem> {
    filesystem: Option<FS>,
    config: Config,
    acl_request_identity: AclRequestIdentity,
    session_owner: Uid,
    mount: Option<UmountOnDrop>,
    channel: Option<Channel>,
    session: Option<Arc<Mutex<Option<Session<FS>>>>>,
    state: SessionOwnerState,
}

/// Result of releasing a construction owner after construction did not finish.
#[derive(Debug)]
#[must_use]
pub enum SessionOwnerShutdown {
    /// Native release did not finish; the same owner remains retryable.
    Pending(io::Error),
    /// Every native owner held by the construction value was released.
    Complete,
}

impl<FS: Filesystem + Send + 'static> SessionOwner<FS> {
    /// Prepare a recoverable construction owner without acquiring native state.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration or mountpoint is invalid.
    pub fn new<P: AsRef<Path>>(
        filesystem: FS,
        mountpoint: P,
        options: &Config,
    ) -> io::Result<Self> {
        let acl_request_identity = validate_mounted_session_config(options)?;
        let mount = Mount::prepare(mountpoint.as_ref())?;
        Ok(Self {
            filesystem: Some(filesystem),
            config: options.clone(),
            acl_request_identity,
            session_owner: geteuid(),
            mount: Some(UmountOnDrop { mount: Some(mount) }),
            channel: None,
            session: None,
            state: SessionOwnerState::Prepared,
        })
    }

    /// Mount, complete INIT, and return a dispatch owner that is not active yet.
    ///
    /// Call [`BackgroundSession::activate`] only after storing the returned
    /// owner. Before activation, the dispatch thread cannot admit ordinary
    /// filesystem requests.
    ///
    /// # Errors
    ///
    /// Returns a construction or thread-spawn error. The caller retains `self`
    /// and must drive [`SessionOwner::shutdown`] until it completes.
    pub fn construct(&mut self) -> io::Result<BackgroundSession> {
        if self.state != SessionOwnerState::Prepared {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "session construction was already attempted",
            ));
        }
        self.state = SessionOwnerState::Constructing;
        self.acquire_and_install_session()?;

        let session = self.session.as_ref().ok_or_else(|| {
            io::Error::other("session construction did not install its session owner")
        })?;
        let (sender, mount) = {
            let mut slot = session.lock();
            let session = slot
                .as_mut()
                .ok_or_else(|| io::Error::other("session construction lost its session owner"))?;
            session.handshake()?;
            let sender = session.ch.sender();
            let mount = std::mem::replace(&mut session.mount, UmountOnDrop::empty());
            (sender, mount)
        };
        self.mount = Some(mount);

        let (activation, activation_receiver) = mpsc::channel();
        let dispatch_session = session.clone();
        let guard = thread::Builder::new()
            .name("fuser-bg".to_string())
            .spawn(move || run_after_activation(dispatch_session, activation_receiver))?;

        let Some(mount) = self.mount.take() else {
            drop(activation);
            return Err(io::Error::other(
                "session construction lost its native mount owner",
            ));
        };
        self.session = None;
        self.state = SessionOwnerState::Transferred;
        Ok(BackgroundSession {
            sender,
            mount,
            dispatch: DispatchOwner::new(guard, activation),
        })
    }

    /// Retry release of every native owner retained after failed construction.
    pub fn shutdown(&mut self, deadline: Instant) -> SessionOwnerShutdown {
        let release = if let Some(mount) = self.mount.as_mut() {
            mount.umount_until(deadline)
        } else if let Some(session) = self.session.as_ref() {
            let mut slot = session.lock();
            match slot.as_mut() {
                Some(session) => session.mount.umount_until(deadline),
                None => Ok(()),
            }
        } else {
            Ok(())
        };

        if let Err(error) = release {
            return SessionOwnerShutdown::Pending(error);
        }

        self.mount = None;
        self.channel = None;
        self.session = None;
        self.filesystem = None;
        self.state = SessionOwnerState::Released;
        SessionOwnerShutdown::Complete
    }

    fn acquire_and_install_session(&mut self) -> io::Result<()> {
        let mount = self
            .mount
            .as_mut()
            .ok_or_else(|| io::Error::other("session construction has no prepared mount owner"))?;
        let channel = {
            let owner = mount
                .mount
                .as_mut()
                .ok_or_else(|| io::Error::other("session construction mount owner was released"))?;
            owner.mount(&self.config.mount_options, self.config.acl)?
        };
        self.channel = Some(channel);

        let filesystem = self
            .filesystem
            .take()
            .ok_or_else(|| io::Error::other("session construction lost its filesystem owner"))?;
        let channel = self
            .channel
            .take()
            .ok_or_else(|| io::Error::other("session construction lost its channel owner"))?;
        let mount = self
            .mount
            .take()
            .ok_or_else(|| io::Error::other("session construction lost its mount owner"))?;
        let session = Session {
            filesystem: FilesystemHolder {
                fs: Some(filesystem),
            },
            ch: channel,
            mount,
            allowed: self.config.acl,
            session_owner: self.session_owner,
            acl_request_identity: self.acl_request_identity,
            proto_version: None,
            config: self.config.clone(),
        };
        self.session = Some(Arc::new(Mutex::new(Some(session))));
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DispatchCommand {
    Activate,
    Stop,
}

fn run_after_activation<FS: Filesystem + Send + 'static>(
    session: Arc<Mutex<Option<Session<FS>>>>,
    activation: Receiver<DispatchCommand>,
) -> io::Result<()> {
    let command = activation.recv().unwrap_or(DispatchCommand::Stop);
    let session = session.lock().take();
    match (command, session) {
        (DispatchCommand::Activate, Some(session)) => session.run(),
        (DispatchCommand::Stop, Some(_)) => Ok(()),
        (_, None) => Err(io::Error::other("dispatch session owner was missing")),
    }
}

pub(crate) struct SessionEventLoop<FS: Filesystem> {
    /// Cache thread name for faster `debug!`.
    pub(crate) thread_name: String,
    pub(crate) ch: Channel,
    pub(crate) filesystem: Arc<FilesystemHolder<FS>>,
    pub(crate) allowed: SessionACL,
    pub(crate) session_owner: Uid,
    pub(crate) acl_request_identity: AclRequestIdentity,
}

impl<FS: Filesystem> SessionEventLoop<FS> {
    fn event_loop(&self) -> io::Result<()> {
        // Buffer for receiving requests from the kernel. Only one is allocated and
        // it is reused immediately after dispatching to conserve memory and allocations.
        let mut buf = FuseReadBuf::new();
        let buf = buf.as_mut();
        loop {
            // Read the next request from the given channel to kernel driver
            // The kernel driver makes sure that we get exactly one request per read
            match self.ch.receive_retrying(buf) {
                Ok(size) => match RequestWithSender::new(self.ch.sender(), &buf[..size]) {
                    // Dispatch request
                    Some(req) => {
                        if let Ok(Operation::Destroy(_)) = req.request.operation() {
                            req.reply::<ReplyEmpty>().ok();
                            return Ok(());
                        } else {
                            req.dispatch(self)
                        }
                    }
                    // Quit loop on illegal request
                    None => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Invalid request",
                        ));
                    }
                },
                Err(nix::errno::Errno::ENODEV) => return Ok(()),
                Err(err) => return Err(err.into()),
            }
        }
    }
}

#[derive(Debug)]
enum DispatchActivation {
    Waiting(Sender<DispatchCommand>),
    Active,
    Stopped,
}

/// Final result of the background dispatch thread.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchOutcome {
    /// The dispatch loop exited successfully.
    Succeeded,
    /// The dispatch loop returned an I/O error.
    Failed {
        /// Stable I/O error category.
        kind: io::ErrorKind,
        /// Error detail returned by the dispatch loop.
        message: String,
    },
    /// The dispatch thread unwound.
    Panicked,
}

/// Exhaustive result of one recoverable background-session shutdown attempt.
#[derive(Debug)]
#[must_use]
pub enum BackgroundSessionShutdown {
    /// The transport was retained after close or unmount did not complete.
    TransportPending(io::Error),
    /// The transport was released but the dispatch guard is not finished yet.
    DispatchPending,
    /// The transport was released and the dispatch guard was reaped.
    Complete(DispatchOutcome),
}

#[derive(Debug)]
struct DispatchOwner {
    guard: Option<JoinHandle<io::Result<()>>>,
    activation: DispatchActivation,
    terminal: Option<DispatchOutcome>,
}

impl DispatchOwner {
    fn new(guard: JoinHandle<io::Result<()>>, activation: Sender<DispatchCommand>) -> Self {
        Self {
            guard: Some(guard),
            activation: DispatchActivation::Waiting(activation),
            terminal: None,
        }
    }

    fn activate(&mut self) -> io::Result<()> {
        let result = match &self.activation {
            DispatchActivation::Waiting(activation) => activation.send(DispatchCommand::Activate),
            DispatchActivation::Active => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "filesystem dispatch is already active",
                ));
            }
            DispatchActivation::Stopped => {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "filesystem dispatch was stopped",
                ));
            }
        };
        match result {
            Ok(()) => {
                self.activation = DispatchActivation::Active;
                Ok(())
            }
            Err(_) => {
                self.activation = DispatchActivation::Stopped;
                Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "filesystem dispatch thread exited before activation",
                ))
            }
        }
    }

    fn shutdown_with(
        &mut self,
        deadline: Instant,
        release_transport: impl FnOnce() -> io::Result<()>,
    ) -> BackgroundSessionShutdown {
        if let Some(outcome) = self.terminal.as_ref() {
            return BackgroundSessionShutdown::Complete(outcome.clone());
        }
        if let Err(error) = release_transport() {
            return BackgroundSessionShutdown::TransportPending(error);
        }

        if let DispatchActivation::Waiting(activation) = &self.activation {
            let _ = activation.send(DispatchCommand::Stop);
            self.activation = DispatchActivation::Stopped;
        }

        let Some(guard) = self.guard.as_ref() else {
            return self.complete_missing_guard();
        };
        while !guard.is_finished() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return BackgroundSessionShutdown::DispatchPending;
            }
            thread::park_timeout(Duration::from_millis(1).min(remaining));
        }

        let Some(guard) = self.guard.take() else {
            return self.complete_missing_guard();
        };
        let outcome = match guard.join() {
            Ok(Ok(())) => DispatchOutcome::Succeeded,
            Ok(Err(error)) => DispatchOutcome::Failed {
                kind: error.kind(),
                message: error.to_string(),
            },
            Err(_) => DispatchOutcome::Panicked,
        };
        self.terminal = Some(outcome.clone());
        BackgroundSessionShutdown::Complete(outcome)
    }

    fn complete_missing_guard(&mut self) -> BackgroundSessionShutdown {
        let outcome = DispatchOutcome::Failed {
            kind: io::ErrorKind::Other,
            message: "filesystem dispatch guard was missing".to_string(),
        };
        self.terminal = Some(outcome.clone());
        BackgroundSessionShutdown::Complete(outcome)
    }
}

/// Non-cloneable owner of a mounted transport and its background dispatch guard.
#[derive(Debug)]
pub struct BackgroundSession {
    sender: ChannelSender,
    mount: UmountOnDrop,
    dispatch: DispatchOwner,
}

impl BackgroundSession {
    /// Admit ordinary filesystem requests after the caller has stored this owner.
    ///
    /// # Errors
    ///
    /// Returns an error if dispatch was already activated or its thread exited
    /// before activation. The session remains available for shutdown.
    pub fn activate(&mut self) -> io::Result<()> {
        self.dispatch.activate()
    }

    /// Release the transport first, then reap dispatch within one shared deadline.
    ///
    /// A non-complete result consumes no native owner. Call this method again on
    /// the same value to resume shutdown.
    pub fn shutdown(&mut self, deadline: Instant) -> BackgroundSessionShutdown {
        let mount = &mut self.mount;
        let sender = &self.sender;
        self.dispatch.shutdown_with(deadline, || {
            mount.umount_until(deadline)?;
            sender.interrupt_receive();
            Ok(())
        })
    }

    /// Returns an object that can be used to send notifications to the kernel.
    ///
    /// The notifier does not extend the transport lifetime. Notifications sent
    /// after the session closes return [`io::ErrorKind::NotConnected`].
    pub fn notifier(&self) -> Notifier {
        Notifier::new(self.sender.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::SyncSender;

    use super::*;

    fn dispatch_owner(task: impl FnOnce() -> io::Result<()> + Send + 'static) -> DispatchOwner {
        let (activation, commands) = mpsc::channel();
        let guard = thread::spawn(
            move || match commands.recv().unwrap_or(DispatchCommand::Stop) {
                DispatchCommand::Activate => task(),
                DispatchCommand::Stop => Ok(()),
            },
        );
        DispatchOwner::new(guard, activation)
    }

    fn blocking_dispatch() -> (DispatchOwner, Receiver<()>, SyncSender<()>) {
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let owner = dispatch_owner(move || {
            entered_tx.send(()).map_err(io::Error::other)?;
            release_rx.recv().map_err(io::Error::other)?;
            Ok(())
        });
        (owner, entered_rx, release_tx)
    }

    #[test]
    fn dispatch_does_not_run_before_explicit_activation() {
        let ran = Arc::new(AtomicBool::new(false));
        let ran_in_dispatch = ran.clone();
        let mut owner = dispatch_owner(move || {
            ran_in_dispatch.store(true, Ordering::SeqCst);
            Ok(())
        });

        assert!(!ran.load(Ordering::SeqCst));
        owner.activate().unwrap();
        let result = owner.shutdown_with(Instant::now() + Duration::from_secs(1), || Ok(()));

        assert!(ran.load(Ordering::SeqCst));
        assert!(matches!(
            result,
            BackgroundSessionShutdown::Complete(DispatchOutcome::Succeeded)
        ));
    }

    #[test]
    fn transport_error_retains_unstarted_dispatch_for_retry() {
        let mut owner = dispatch_owner(|| Ok(()));

        let first = owner.shutdown_with(Instant::now(), || {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "synthetic Linux unmount failure",
            ))
        });
        assert!(matches!(
            first,
            BackgroundSessionShutdown::TransportPending(error)
                if error.kind() == io::ErrorKind::PermissionDenied
        ));

        let second = owner.shutdown_with(Instant::now() + Duration::from_secs(1), || Ok(()));
        assert!(matches!(
            second,
            BackgroundSessionShutdown::Complete(DispatchOutcome::Succeeded)
        ));
    }

    #[test]
    fn released_transport_with_unfinished_dispatch_remains_retryable() {
        let (mut owner, entered, release) = blocking_dispatch();
        owner.activate().unwrap();
        entered.recv().unwrap();

        let first = owner.shutdown_with(Instant::now(), || Ok(()));
        assert!(matches!(first, BackgroundSessionShutdown::DispatchPending));

        release.send(()).unwrap();
        let second = owner.shutdown_with(Instant::now() + Duration::from_secs(1), || Ok(()));
        assert!(matches!(
            second,
            BackgroundSessionShutdown::Complete(DispatchOutcome::Succeeded)
        ));
    }

    #[test]
    fn worker_unwind_is_reaped_after_transport_release() {
        let mut owner = dispatch_owner(|| panic!("synthetic dispatch unwind"));
        owner.activate().unwrap();

        let pending = owner.shutdown_with(Instant::now() + Duration::from_secs(1), || {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "synthetic retained transport",
            ))
        });
        assert!(matches!(
            pending,
            BackgroundSessionShutdown::TransportPending(error)
                if error.kind() == io::ErrorKind::WouldBlock
        ));

        let result = owner.shutdown_with(Instant::now() + Duration::from_secs(1), || Ok(()));

        assert!(matches!(
            result,
            BackgroundSessionShutdown::Complete(DispatchOutcome::Panicked)
        ));
    }

    #[test]
    fn dispatch_io_error_is_preserved_as_terminal_outcome() {
        let mut owner = dispatch_owner(|| {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "synthetic dispatch failure",
            ))
        });
        owner.activate().unwrap();

        let result = owner.shutdown_with(Instant::now() + Duration::from_secs(1), || Ok(()));

        assert!(matches!(
            result,
            BackgroundSessionShutdown::Complete(DispatchOutcome::Failed {
                kind: io::ErrorKind::InvalidData,
                ref message,
            }) if message == "synthetic dispatch failure"
        ));
    }
}
