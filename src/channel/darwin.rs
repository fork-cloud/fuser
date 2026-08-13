use std::fmt::Debug;
use std::io;
use std::io::IoSlice;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use parking_lot::Condvar;
use parking_lot::Mutex;

const WAKE_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ActiveCalls {
    receives: usize,
    sends: usize,
}

impl ActiveCalls {
    fn is_empty(self) -> bool {
        self.receives == 0 && self.sends == 0
    }

    fn admit(&mut self, kind: CallKind) -> io::Result<()> {
        let active = match kind {
            CallKind::Receive => &mut self.receives,
            CallKind::Send => &mut self.sends,
        };
        *active = active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("macFUSE active call count overflowed"))?;
        Ok(())
    }

    fn finish(&mut self, kind: CallKind) {
        let active = match kind {
            CallKind::Receive => &mut self.receives,
            CallKind::Send => &mut self.sends,
        };
        *active = active
            .checked_sub(1)
            .unwrap_or_else(|| unreachable!("an admitted macFUSE call must be active"));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CallKind {
    Receive,
    Send,
}

#[derive(Debug)]
enum Lifecycle<Session, Channel> {
    Open {
        session: Session,
        channel: Channel,
        active: ActiveCalls,
    },
    Closing {
        session: Session,
        channel: Channel,
        active: ActiveCalls,
    },
    Closed,
}

impl<Session, Channel> Lifecycle<Session, Channel> {
    fn active_mut(&mut self) -> Option<&mut ActiveCalls> {
        match self {
            Self::Open { active, .. } | Self::Closing { active, .. } => Some(active),
            Self::Closed => None,
        }
    }
}

trait DarwinChannelApi: Debug + Send + Sync + 'static {
    type Session: Copy + Debug + Send + Sync + 'static;
    type Channel: Copy + Debug + Send + Sync + 'static;

    fn receive(&self, channel: Self::Channel, buffer: &mut [u8]) -> nix::Result<usize>;
    fn send(&self, channel: Self::Channel, buffers: &[IoSlice<'_>]) -> io::Result<()>;
    fn session_exit(&self, session: Self::Session);
    fn interrupt(&self, channel: Self::Channel);
    fn session_remove_channel(&self, channel: Self::Channel);
    fn session_destroy(&self, session: Self::Session);
    fn darwin_unmount(&self, channel: Self::Channel);
    fn channel_destroy(&self, channel: Self::Channel);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ShutdownTiming {
    wake_interval: Duration,
}

impl Default for ShutdownTiming {
    fn default() -> Self {
        Self {
            wake_interval: WAKE_INTERVAL,
        }
    }
}

#[derive(Debug)]
struct Shared<A: DarwinChannelApi> {
    api: A,
    lifecycle: Mutex<Lifecycle<A::Session, A::Channel>>,
    active_changed: Condvar,
    timing: ShutdownTiming,
}

impl<A: DarwinChannelApi> Shared<A> {
    fn new(api: A, session: A::Session, channel: A::Channel, timing: ShutdownTiming) -> Arc<Self> {
        assert!(!timing.wake_interval.is_zero());
        Arc::new(Self {
            api,
            lifecycle: Mutex::new(Lifecycle::Open {
                session,
                channel,
                active: ActiveCalls::default(),
            }),
            active_changed: Condvar::new(),
            timing,
        })
    }

    fn receive(self: &Arc<Self>, buffer: &mut [u8]) -> nix::Result<usize> {
        let permit = self.admit(CallKind::Receive).map_err(io_error_to_errno)?;
        let result = self.api.receive(permit.channel, buffer);
        match result {
            Ok(0) => Err(nix::errno::Errno::ENODEV),
            result => result,
        }
    }

    fn send(self: &Arc<Self>, buffers: &[IoSlice<'_>]) -> io::Result<()> {
        let permit = self.admit(CallKind::Send)?;
        self.api.send(permit.channel, buffers)
    }

    fn close(&self, deadline: Instant) -> io::Result<()> {
        let Some((session, channel, exit_session)) = self.begin_close() else {
            return Ok(());
        };
        if exit_session {
            self.api.session_exit(session);
        }

        loop {
            let active = {
                let lifecycle = self.lifecycle.lock();
                match &*lifecycle {
                    Lifecycle::Closing { active, .. } => *active,
                    Lifecycle::Closed => return Ok(()),
                    Lifecycle::Open { .. } => unreachable!("close must transition to Closing"),
                }
            };

            if active.is_empty() {
                break;
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(drain_timeout_error());
            }
            if active.receives != 0 {
                self.api.interrupt(channel);
            }

            let mut lifecycle = self.lifecycle.lock();
            let active = match &*lifecycle {
                Lifecycle::Closing { active, .. } => *active,
                Lifecycle::Closed => return Ok(()),
                Lifecycle::Open { .. } => unreachable!("close must remain in Closing"),
            };
            if active.is_empty() {
                continue;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(drain_timeout_error());
            }
            self.active_changed
                .wait_for(&mut lifecycle, self.timing.wake_interval.min(remaining));
        }

        let (session, channel) = self.drained_handles();
        self.api.session_remove_channel(channel);
        self.api.session_destroy(session);
        self.api.darwin_unmount(channel);
        self.api.channel_destroy(channel);

        let mut lifecycle = self.lifecycle.lock();
        match &*lifecycle {
            Lifecycle::Closing { active, .. } if active.is_empty() => {
                *lifecycle = Lifecycle::Closed
            }
            Lifecycle::Closing { .. } => {
                unreachable!("successful drain must remain inactive")
            }
            Lifecycle::Closed => {}
            Lifecycle::Open { .. } => unreachable!("close must remain in Closing"),
        }
        Ok(())
    }

    fn drained_handles(&self) -> (A::Session, A::Channel) {
        let lifecycle = self.lifecycle.lock();
        match &*lifecycle {
            Lifecycle::Closing {
                session,
                channel,
                active,
            } if active.is_empty() => (*session, *channel),
            Lifecycle::Closing { .. } => {
                unreachable!("only an inactive channel can be released")
            }
            Lifecycle::Open { .. } => unreachable!("release requires Closing state"),
            Lifecycle::Closed => unreachable!("Closed state has no provider handles"),
        }
    }

    fn begin_close(&self) -> Option<(A::Session, A::Channel, bool)> {
        let mut lifecycle = self.lifecycle.lock();
        match &*lifecycle {
            Lifecycle::Open {
                session,
                channel,
                active,
            } => {
                let (session, channel, active) = (*session, *channel, *active);
                *lifecycle = Lifecycle::Closing {
                    session,
                    channel,
                    active,
                };
                Some((session, channel, true))
            }
            Lifecycle::Closing {
                session, channel, ..
            } => Some((*session, *channel, false)),
            Lifecycle::Closed => None,
        }
    }

    fn admit(self: &Arc<Self>, kind: CallKind) -> io::Result<CallPermit<A>> {
        let channel = {
            let mut lifecycle = self.lifecycle.lock();
            match &mut *lifecycle {
                Lifecycle::Open {
                    channel, active, ..
                } => {
                    active.admit(kind)?;
                    *channel
                }
                Lifecycle::Closing { .. } | Lifecycle::Closed => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "macFUSE channel is closing",
                    ));
                }
            }
        };
        Ok(CallPermit {
            shared: self.clone(),
            channel,
            kind,
        })
    }

    fn finish_call(&self, kind: CallKind) {
        let mut lifecycle = self.lifecycle.lock();
        let Some(active) = lifecycle.active_mut() else {
            unreachable!("an admitted call cannot outlive Closed state")
        };
        active.finish(kind);
        self.active_changed.notify_all();
    }
}

struct CallPermit<A: DarwinChannelApi> {
    shared: Arc<Shared<A>>,
    channel: A::Channel,
    kind: CallKind,
}

impl<A: DarwinChannelApi> Drop for CallPermit<A> {
    fn drop(&mut self) {
        self.shared.finish_call(self.kind);
    }
}

fn drain_timeout_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "macFUSE channel calls did not drain before the deadline; provider state was retained",
    )
}

fn io_error_to_errno(error: io::Error) -> nix::errno::Errno {
    error
        .raw_os_error()
        .map(nix::errno::Errno::from_raw)
        .unwrap_or(nix::errno::Errno::ENODEV)
}

#[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
mod real {
    use std::cmp::Ordering;
    use std::ffi::c_void;
    use std::os::raw::c_char;
    use std::ptr;
    use std::ptr::NonNull;

    use smallvec::SmallVec;

    use super::*;
    use crate::mnt::fuse2_sys;
    use crate::mnt::fuse2_sys::fuse_args;
    use crate::mnt::fuse2_sys::fuse_chan;
    use crate::mnt::fuse2_sys::fuse_session;
    use crate::mnt::fuse2_sys::fuse_session_ops;

    #[derive(Clone, Copy, Debug)]
    struct SessionHandle(NonNull<fuse_session>);

    // SAFETY: access is serialized by Shared and libfuse's session is kept alive
    // until every admitted channel call has returned.
    unsafe impl Send for SessionHandle {}
    // SAFETY: only the lifecycle owner invokes session operations.
    unsafe impl Sync for SessionHandle {}

    #[derive(Clone, Copy, Debug)]
    struct ChannelHandle(NonNull<fuse_chan>);

    // SAFETY: libfuse permits concurrent replies, and Shared prevents teardown
    // while any admitted receive or send is using the pointer.
    unsafe impl Send for ChannelHandle {}
    // SAFETY: all lifecycle mutation is single-owner and admission is locked.
    unsafe impl Sync for ChannelHandle {}

    #[derive(Debug)]
    struct RealApi;

    impl DarwinChannelApi for RealApi {
        type Session = SessionHandle;
        type Channel = ChannelHandle;

        fn receive(&self, channel: ChannelHandle, buffer: &mut [u8]) -> nix::Result<usize> {
            let mut raw_channel = channel.0.as_ptr();
            // SAFETY: admission keeps the channel alive, and `buffer` is writable
            // for its reported length for the duration of this call.
            let result = unsafe {
                fuse2_sys::fuse_chan_recv(
                    &mut raw_channel,
                    buffer.as_mut_ptr().cast::<c_char>(),
                    buffer.len(),
                )
            };
            if result < 0 {
                match result.checked_neg() {
                    Some(errno) => Err(nix::errno::Errno::from_raw(errno)),
                    None => Err(nix::errno::Errno::EIO),
                }
            } else if raw_channel != channel.0.as_ptr() || result as usize > buffer.len() {
                Err(nix::errno::Errno::EIO)
            } else {
                Ok(result as usize)
            }
        }

        fn send(&self, channel: ChannelHandle, buffers: &[IoSlice<'_>]) -> io::Result<()> {
            let iovecs: SmallVec<[libc::iovec; 4]> = buffers
                .iter()
                .map(|buffer| libc::iovec {
                    iov_base: buffer.as_ptr().cast_mut().cast::<c_void>(),
                    iov_len: buffer.len(),
                })
                .collect();
            // SAFETY: admission keeps the channel alive, and every iovec borrows
            // a live `IoSlice` for the duration of this call.
            let result = unsafe {
                fuse2_sys::fuse_chan_send(channel.0.as_ptr(), iovecs.as_ptr(), iovecs.len())
            };
            match result.cmp(&0) {
                Ordering::Less => result.checked_neg().map_or_else(
                    || {
                        Err(io::Error::other(
                            "fuse_chan_send returned an invalid status",
                        ))
                    },
                    |errno| Err(io::Error::from_raw_os_error(errno)),
                ),
                Ordering::Equal => Ok(()),
                Ordering::Greater => Err(io::Error::other(format!(
                    "fuse_chan_send returned unexpected status {result}"
                ))),
            }
        }

        fn session_exit(&self, session: SessionHandle) {
            // SAFETY: the lifecycle state owns this live session until calls drain.
            unsafe { fuse2_sys::fuse_session_exit(session.0.as_ptr()) }
        }

        fn interrupt(&self, channel: ChannelHandle) {
            // SAFETY: the channel remains live while an admitted receive is active.
            unsafe { fuse2_sys::fuse_darwin_chan_interrupt(channel.0.as_ptr()) }
        }

        fn session_remove_channel(&self, channel: ChannelHandle) {
            // SAFETY: all admitted calls drained before the owner detaches the channel.
            unsafe { fuse2_sys::fuse_session_remove_chan(channel.0.as_ptr()) }
        }

        fn session_destroy(&self, session: SessionHandle) {
            // SAFETY: the channel was detached and no admitted call can use the session.
            unsafe { fuse2_sys::fuse_session_destroy(session.0.as_ptr()) }
        }

        fn darwin_unmount(&self, channel: ChannelHandle) {
            // SAFETY: the detached live channel still owns its provider mount state.
            unsafe { fuse2_sys::fuse_darwin_chan_unmount(channel.0.as_ptr()) }
        }

        fn channel_destroy(&self, channel: ChannelHandle) {
            // SAFETY: Darwin unmount retained any asynchronous worker reference, so
            // this releases exactly the Rust owner's live channel reference.
            unsafe { fuse2_sys::fuse_chan_destroy(channel.0.as_ptr()) }
        }
    }

    #[derive(Clone, Debug)]
    pub(crate) struct MountedChannel(Arc<Shared<RealApi>>);

    impl MountedChannel {
        pub(crate) fn receive(&self, buffer: &mut [u8]) -> nix::Result<usize> {
            self.0.receive(buffer)
        }

        pub(crate) fn sender(&self) -> MountedSender {
            MountedSender(self.0.clone())
        }
    }

    #[derive(Clone, Debug)]
    pub(crate) struct MountedSender(Arc<Shared<RealApi>>);

    impl MountedSender {
        pub(crate) fn send(&self, buffers: &[IoSlice<'_>]) -> io::Result<()> {
            self.0.send(buffers)
        }
    }

    #[derive(Debug)]
    enum MountedChannelOwnerState {
        Empty,
        Channel(ChannelHandle),
        UnattachedSession {
            session: SessionHandle,
            channel: ChannelHandle,
        },
        AttachedSession {
            session: SessionHandle,
            channel: ChannelHandle,
        },
        Mounted(Arc<Shared<RealApi>>),
        Released,
    }

    #[derive(Debug)]
    pub(crate) struct MountedChannelOwner {
        state: MountedChannelOwnerState,
    }

    impl MountedChannelOwner {
        pub(crate) fn new() -> Self {
            Self {
                state: MountedChannelOwnerState::Empty,
            }
        }

        pub(crate) fn mount(
            &mut self,
            mountpoint: &std::ffi::CStr,
            args: &mut fuse_args,
        ) -> io::Result<MountedChannel> {
            if !matches!(self.state, MountedChannelOwnerState::Empty) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "macFUSE channel owner was already used",
                ));
            }
            // SAFETY: `mountpoint` and every argv string remain live through the call;
            // `args` is exclusively borrowed because libfuse rewrites it while parsing.
            let raw_channel = unsafe { fuse2_sys::fuse_mount(mountpoint.as_ptr(), args) };
            if raw_channel.is_null() {
                let error = ensure_last_os_error();
                // SAFETY: libfuse initialized `args`; this releases only argv storage
                // that libfuse allocated while preserving the Rust-owned input strings.
                unsafe { fuse2_sys::fuse_opt_free_args(args) }
                return Err(error);
            }
            // SAFETY: same ownership rule as the error branch above.
            unsafe { fuse2_sys::fuse_opt_free_args(args) }
            // SAFETY: the null case returned above.
            let channel = unsafe { NonNull::new_unchecked(raw_channel) };
            let channel = ChannelHandle(channel);
            self.state = MountedChannelOwnerState::Channel(channel);

            let mut ops = fuse_session_ops {
                process: Some(ignore_request),
                exit: None,
                exited: None,
                destroy: None,
            };
            // SAFETY: libfuse copies `ops` during this call and receives no user data.
            let Some(session) =
                NonNull::new(unsafe { fuse2_sys::fuse_session_new(&mut ops, ptr::null_mut()) })
            else {
                let error = ensure_last_os_error();
                self.close(Instant::now())?;
                return Err(error);
            };
            let session = SessionHandle(session);
            self.state = MountedChannelOwnerState::UnattachedSession { session, channel };

            // SAFETY: both objects are live, unattached, and exclusively lifecycle-owned.
            unsafe { fuse2_sys::fuse_session_add_chan(session.0.as_ptr(), channel.0.as_ptr()) }
            self.state = MountedChannelOwnerState::AttachedSession { session, channel };
            let shared = Shared::new(RealApi, session, channel, ShutdownTiming::default());
            self.state = MountedChannelOwnerState::Mounted(shared.clone());
            Ok(MountedChannel(shared))
        }

        pub(crate) fn close(&mut self, deadline: Instant) -> io::Result<()> {
            match &self.state {
                MountedChannelOwnerState::Empty => {
                    self.state = MountedChannelOwnerState::Released;
                    Ok(())
                }
                MountedChannelOwnerState::Channel(channel) => {
                    let channel = *channel;
                    // SAFETY: the unattached channel is live and exclusively owned.
                    unsafe {
                        fuse2_sys::fuse_darwin_chan_unmount(channel.0.as_ptr());
                        fuse2_sys::fuse_chan_destroy(channel.0.as_ptr());
                    }
                    self.state = MountedChannelOwnerState::Released;
                    Ok(())
                }
                MountedChannelOwnerState::UnattachedSession { session, channel } => {
                    let (session, channel) = (*session, *channel);
                    // SAFETY: neither owner is attached or visible to a request loop.
                    unsafe {
                        fuse2_sys::fuse_session_destroy(session.0.as_ptr());
                        fuse2_sys::fuse_darwin_chan_unmount(channel.0.as_ptr());
                        fuse2_sys::fuse_chan_destroy(channel.0.as_ptr());
                    }
                    self.state = MountedChannelOwnerState::Released;
                    Ok(())
                }
                MountedChannelOwnerState::AttachedSession { session, channel } => {
                    let (session, channel) = (*session, *channel);
                    // SAFETY: no channel clone was published before Shared was installed.
                    unsafe {
                        fuse2_sys::fuse_session_remove_chan(channel.0.as_ptr());
                        fuse2_sys::fuse_session_destroy(session.0.as_ptr());
                        fuse2_sys::fuse_darwin_chan_unmount(channel.0.as_ptr());
                        fuse2_sys::fuse_chan_destroy(channel.0.as_ptr());
                    }
                    self.state = MountedChannelOwnerState::Released;
                    Ok(())
                }
                MountedChannelOwnerState::Mounted(shared) => {
                    shared.close(deadline)?;
                    self.state = MountedChannelOwnerState::Released;
                    Ok(())
                }
                MountedChannelOwnerState::Released => Ok(()),
            }
        }
    }

    unsafe extern "C" fn ignore_request(
        _data: *mut c_void,
        _buffer: *const c_char,
        _length: usize,
        _channel: *mut fuse_chan,
    ) {
    }

    fn ensure_last_os_error() -> io::Error {
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(0) | None => io::Error::other("libfuse returned no error detail"),
            Some(_) => error,
        }
    }
}

#[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
pub(crate) use real::MountedChannel;
#[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
pub(crate) use real::MountedChannelOwner;
#[cfg(all(target_os = "macos", fuser_mount_impl = "libfuse2"))]
pub(crate) use real::MountedSender;

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Condvar as StdCondvar;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::thread;

    use super::*;

    const SESSION: usize = 11;
    const CHANNEL: usize = 29;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum UnmountMode {
        Synchronous,
        Asynchronous,
        WorkerCreationFailure,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ReceiveMode {
        Return,
        LoseFirstWake,
        IgnoreInterrupts,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Event {
        ReceiveEntered,
        Send,
        SessionExit,
        Interrupt,
        SessionRemoveChannel,
        SessionDestroy,
        DarwinUnmount,
        ChannelRetain,
        ChannelRelease,
        ChannelDestroy,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ParsedOperation {
        Init,
        GetAttr,
    }

    #[derive(Debug)]
    struct ReceiveState {
        mode: ReceiveMode,
        entered: bool,
        interrupt_generation: usize,
        released: bool,
    }

    #[derive(Debug)]
    struct SendState {
        blocked: bool,
        entered: bool,
        released: bool,
    }

    #[derive(Debug)]
    struct FakeState {
        events: StdMutex<Vec<Event>>,
        event_changed: StdCondvar,
        receive: StdMutex<ReceiveState>,
        receive_changed: StdCondvar,
        messages: StdMutex<VecDeque<Vec<u8>>>,
        send: StdMutex<SendState>,
        send_changed: StdCondvar,
        sent_messages: StdMutex<Vec<Vec<Vec<u8>>>>,
        unmount_mode: UnmountMode,
        channel_references: AtomicUsize,
        channel_attached: AtomicBool,
    }

    #[derive(Clone, Debug)]
    struct FakeApi(Arc<FakeState>);

    impl FakeApi {
        fn new(unmount_mode: UnmountMode, receive_mode: ReceiveMode) -> Self {
            Self::configured(unmount_mode, receive_mode, false, VecDeque::new())
        }

        fn with_messages(messages: impl IntoIterator<Item = Vec<u8>>) -> Self {
            Self::configured(
                UnmountMode::Synchronous,
                ReceiveMode::Return,
                false,
                messages.into_iter().collect(),
            )
        }

        fn with_blocked_send() -> Self {
            Self::configured(
                UnmountMode::Synchronous,
                ReceiveMode::Return,
                true,
                VecDeque::new(),
            )
        }

        fn configured(
            unmount_mode: UnmountMode,
            receive_mode: ReceiveMode,
            send_blocked: bool,
            messages: VecDeque<Vec<u8>>,
        ) -> Self {
            Self(Arc::new(FakeState {
                events: StdMutex::new(Vec::new()),
                event_changed: StdCondvar::new(),
                receive: StdMutex::new(ReceiveState {
                    mode: receive_mode,
                    entered: false,
                    interrupt_generation: 0,
                    released: false,
                }),
                receive_changed: StdCondvar::new(),
                messages: StdMutex::new(messages),
                send: StdMutex::new(SendState {
                    blocked: send_blocked,
                    entered: false,
                    released: false,
                }),
                send_changed: StdCondvar::new(),
                sent_messages: StdMutex::new(Vec::new()),
                unmount_mode,
                channel_references: AtomicUsize::new(1),
                channel_attached: AtomicBool::new(true),
            }))
        }

        fn record(&self, event: Event) {
            self.0.events.lock().unwrap().push(event);
            self.0.event_changed.notify_all();
        }

        fn events(&self) -> Vec<Event> {
            self.0.events.lock().unwrap().clone()
        }

        fn wait_for_event(&self, expected: Event) {
            let deadline = Instant::now() + Duration::from_secs(1);
            let mut events = self.0.events.lock().unwrap();
            while !events.contains(&expected) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "event {expected:?} was not recorded");
                let (next, timeout) = self
                    .0
                    .event_changed
                    .wait_timeout(events, remaining)
                    .unwrap();
                events = next;
                assert!(!timeout.timed_out() || events.contains(&expected));
            }
        }

        fn wait_for_receive_entry(&self) {
            let deadline = Instant::now() + Duration::from_secs(1);
            let mut receive = self.0.receive.lock().unwrap();
            while !receive.entered {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !remaining.is_zero(),
                    "receive did not enter within one second"
                );
                let (next, timeout) = self
                    .0
                    .receive_changed
                    .wait_timeout(receive, remaining)
                    .unwrap();
                receive = next;
                assert!(!timeout.timed_out() || receive.entered);
            }
        }

        fn release_receive(&self) {
            let mut receive = self.0.receive.lock().unwrap();
            receive.released = true;
            self.0.receive_changed.notify_all();
        }

        fn wait_for_send_entry(&self) {
            let deadline = Instant::now() + Duration::from_secs(1);
            let mut send = self.0.send.lock().unwrap();
            while !send.entered {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "send did not enter within one second");
                let (next, timeout) = self.0.send_changed.wait_timeout(send, remaining).unwrap();
                send = next;
                assert!(!timeout.timed_out() || send.entered);
            }
        }

        fn release_send(&self) {
            let mut send = self.0.send.lock().unwrap();
            send.released = true;
            self.0.send_changed.notify_all();
        }

        fn sent_messages(&self) -> Vec<Vec<Vec<u8>>> {
            self.0.sent_messages.lock().unwrap().clone()
        }

        fn channel_references(&self) -> usize {
            self.0.channel_references.load(Ordering::SeqCst)
        }

        fn complete_async_worker(&self) {
            assert_eq!(self.0.unmount_mode, UnmountMode::Asynchronous);
            let previous = self.0.channel_references.fetch_sub(1, Ordering::SeqCst);
            assert_eq!(previous, 1);
            self.record(Event::ChannelRelease);
        }
    }

    impl DarwinChannelApi for FakeApi {
        type Session = usize;
        type Channel = usize;

        fn receive(&self, channel: usize, buffer: &mut [u8]) -> nix::Result<usize> {
            assert_eq!(channel, CHANNEL);
            let mut receive = self.0.receive.lock().unwrap();
            receive.entered = true;
            self.record(Event::ReceiveEntered);
            self.0.receive_changed.notify_all();

            match receive.mode {
                ReceiveMode::Return => {
                    drop(receive);
                    let Some(message) = self.0.messages.lock().unwrap().pop_front() else {
                        return Ok(0);
                    };
                    if message.len() > buffer.len() {
                        return Err(nix::errno::Errno::EOVERFLOW);
                    }
                    buffer[..message.len()].copy_from_slice(&message);
                    Ok(message.len())
                }
                ReceiveMode::LoseFirstWake => {
                    while receive.interrupt_generation == 0 {
                        receive = self.0.receive_changed.wait(receive).unwrap();
                    }
                    let observed_generation = receive.interrupt_generation;
                    while !receive.released && receive.interrupt_generation == observed_generation {
                        receive = self.0.receive_changed.wait(receive).unwrap();
                    }
                    Ok(0)
                }
                ReceiveMode::IgnoreInterrupts => {
                    while !receive.released {
                        receive = self.0.receive_changed.wait(receive).unwrap();
                    }
                    Ok(0)
                }
            }
        }

        fn send(&self, channel: usize, buffers: &[IoSlice<'_>]) -> io::Result<()> {
            assert_eq!(channel, CHANNEL);
            let mut send = self.0.send.lock().unwrap();
            send.entered = true;
            self.record(Event::Send);
            self.0.send_changed.notify_all();
            while send.blocked && !send.released {
                send = self.0.send_changed.wait(send).unwrap();
            }
            self.0.sent_messages.lock().unwrap().push(
                buffers
                    .iter()
                    .map(|buffer| buffer.as_ref().to_vec())
                    .collect(),
            );
            Ok(())
        }

        fn session_exit(&self, session: usize) {
            assert_eq!(session, SESSION);
            self.record(Event::SessionExit);
        }

        fn interrupt(&self, channel: usize) {
            assert_eq!(channel, CHANNEL);
            self.record(Event::Interrupt);
            let mut receive = self.0.receive.lock().unwrap();
            receive.interrupt_generation += 1;
            self.0.receive_changed.notify_all();
        }

        fn session_remove_channel(&self, channel: usize) {
            assert_eq!(channel, CHANNEL);
            assert!(self.0.channel_attached.swap(false, Ordering::SeqCst));
            self.record(Event::SessionRemoveChannel);
        }

        fn session_destroy(&self, session: usize) {
            assert_eq!(session, SESSION);
            assert!(!self.0.channel_attached.load(Ordering::SeqCst));
            self.record(Event::SessionDestroy);
        }

        fn darwin_unmount(&self, channel: usize) {
            assert_eq!(channel, CHANNEL);
            assert!(!self.0.channel_attached.load(Ordering::SeqCst));
            self.record(Event::DarwinUnmount);
            match self.0.unmount_mode {
                UnmountMode::Synchronous => {}
                UnmountMode::Asynchronous => {
                    self.0.channel_references.fetch_add(1, Ordering::SeqCst);
                    self.record(Event::ChannelRetain);
                }
                UnmountMode::WorkerCreationFailure => {
                    self.0.channel_references.fetch_add(1, Ordering::SeqCst);
                    self.record(Event::ChannelRetain);
                    self.0.channel_references.fetch_sub(1, Ordering::SeqCst);
                    self.record(Event::ChannelRelease);
                }
            }
        }

        fn channel_destroy(&self, channel: usize) {
            assert_eq!(channel, CHANNEL);
            let previous = self.0.channel_references.fetch_sub(1, Ordering::SeqCst);
            assert_ne!(previous, 0);
            self.record(Event::ChannelDestroy);
        }
    }

    fn shared(api: FakeApi, timing: ShutdownTiming) -> Arc<Shared<FakeApi>> {
        Shared::new(api, SESSION, CHANNEL, timing)
    }

    fn lifecycle_events(events: &[Event]) -> Vec<Event> {
        events
            .iter()
            .copied()
            .filter(|event| {
                matches!(
                    event,
                    Event::SessionExit
                        | Event::SessionRemoveChannel
                        | Event::SessionDestroy
                        | Event::DarwinUnmount
                        | Event::ChannelDestroy
                )
            })
            .collect()
    }

    fn request_header(length: usize, opcode: u32, unique: u64) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(length);
        bytes.extend_from_slice(&(length as u32).to_ne_bytes());
        bytes.extend_from_slice(&opcode.to_ne_bytes());
        bytes.extend_from_slice(&unique.to_ne_bytes());
        bytes.extend_from_slice(&1_u64.to_ne_bytes());
        bytes.extend_from_slice(&501_u32.to_ne_bytes());
        bytes.extend_from_slice(&20_u32.to_ne_bytes());
        bytes.extend_from_slice(&1234_u32.to_ne_bytes());
        bytes.extend_from_slice(&0_u32.to_ne_bytes());
        bytes
    }

    fn init_request() -> Vec<u8> {
        let mut bytes = request_header(104, 26, 1);
        bytes.extend_from_slice(&7_u32.to_ne_bytes());
        bytes.extend_from_slice(&31_u32.to_ne_bytes());
        bytes.extend_from_slice(&4096_u32.to_ne_bytes());
        bytes.extend_from_slice(&0_u32.to_ne_bytes());
        bytes.extend_from_slice(&0_u32.to_ne_bytes());
        for _ in 0..11 {
            bytes.extend_from_slice(&0_u32.to_ne_bytes());
        }
        assert_eq!(bytes.len(), 104);
        bytes
    }

    fn getattr_request() -> Vec<u8> {
        let mut bytes = request_header(56, 3, 2);
        bytes.extend_from_slice(&0_u32.to_ne_bytes());
        bytes.extend_from_slice(&0_u32.to_ne_bytes());
        bytes.extend_from_slice(&0_u64.to_ne_bytes());
        assert_eq!(bytes.len(), 56);
        bytes
    }

    #[repr(align(8))]
    struct AlignedBuffer([u8; 128]);

    fn parse_operation(message: &[u8]) -> ParsedOperation {
        let request = crate::ll::AnyRequest::try_from(message).unwrap();
        match request.operation().unwrap() {
            crate::ll::Operation::Init(_) => ParsedOperation::Init,
            crate::ll::Operation::GetAttr(_) => ParsedOperation::GetAttr,
            operation => panic!("unexpected operation: {operation:?}"),
        }
    }

    fn assert_teardown(mode: UnmountMode) {
        let api = FakeApi::new(mode, ReceiveMode::Return);
        let shared = shared(api.clone(), ShutdownTiming::default());

        shared
            .close(Instant::now() + Duration::from_secs(1))
            .unwrap();
        shared
            .close(Instant::now() + Duration::from_secs(1))
            .unwrap();
        if mode == UnmountMode::Asynchronous {
            api.complete_async_worker();
        }

        let expected = match mode {
            UnmountMode::Synchronous => vec![
                Event::SessionExit,
                Event::SessionRemoveChannel,
                Event::SessionDestroy,
                Event::DarwinUnmount,
                Event::ChannelDestroy,
            ],
            UnmountMode::Asynchronous => vec![
                Event::SessionExit,
                Event::SessionRemoveChannel,
                Event::SessionDestroy,
                Event::DarwinUnmount,
                Event::ChannelRetain,
                Event::ChannelDestroy,
                Event::ChannelRelease,
            ],
            UnmountMode::WorkerCreationFailure => vec![
                Event::SessionExit,
                Event::SessionRemoveChannel,
                Event::SessionDestroy,
                Event::DarwinUnmount,
                Event::ChannelRetain,
                Event::ChannelRelease,
                Event::ChannelDestroy,
            ],
        };
        assert_eq!(api.events(), expected);
        assert_eq!(api.channel_references(), 0);
    }

    #[test]
    fn production_shutdown_timing_is_bounded() {
        assert_eq!(
            ShutdownTiming::default(),
            ShutdownTiming {
                wake_interval: Duration::from_millis(25),
            }
        );
    }

    #[test]
    fn distinct_init_and_getattr_messages_reach_parser_once_each() {
        let api = FakeApi::with_messages([init_request(), getattr_request()]);
        let shared = shared(api, ShutdownTiming::default());
        let mut buffer = AlignedBuffer([0; 128]);
        let mut parsed = Vec::new();

        for _ in 0..2 {
            let size = shared.receive(&mut buffer.0).unwrap();
            parsed.push(parse_operation(&buffer.0[..size]));
        }

        assert_eq!(parsed, [ParsedOperation::Init, ParsedOperation::GetAttr]);
    }

    #[test]
    fn vectored_reply_reaches_provider_as_one_message_with_original_segments() {
        let api = FakeApi::new(UnmountMode::Synchronous, ReceiveMode::Return);
        let shared = shared(api.clone(), ShutdownTiming::default());

        shared
            .send(&[IoSlice::new(b"header"), IoSlice::new(b"payload")])
            .unwrap();

        assert_eq!(
            api.sent_messages(),
            [vec![b"header".to_vec(), b"payload".to_vec()]]
        );
    }

    #[test]
    fn synchronous_teardown_releases_the_owner_reference_once() {
        assert_teardown(UnmountMode::Synchronous);
    }

    #[test]
    fn asynchronous_teardown_preserves_and_releases_the_worker_reference() {
        assert_teardown(UnmountMode::Asynchronous);
    }

    #[test]
    fn worker_creation_failure_releases_the_temporary_reference() {
        assert_teardown(UnmountMode::WorkerCreationFailure);
    }

    #[test]
    fn repeated_interrupt_closes_pre_block_lost_wakeup() {
        let api = FakeApi::new(UnmountMode::Synchronous, ReceiveMode::LoseFirstWake);
        let shared = shared(
            api.clone(),
            ShutdownTiming {
                wake_interval: Duration::from_millis(5),
            },
        );
        let receiver = {
            let shared = shared.clone();
            thread::spawn(move || {
                let mut buffer = [0_u8; 64];
                assert_eq!(shared.receive(&mut buffer), Err(nix::errno::Errno::ENODEV));
            })
        };
        api.wait_for_receive_entry();

        shared
            .close(Instant::now() + Duration::from_secs(1))
            .unwrap();
        receiver.join().unwrap();

        assert!(
            api.events()
                .iter()
                .filter(|event| **event == Event::Interrupt)
                .count()
                >= 2
        );
        assert_eq!(api.channel_references(), 0);
    }

    #[test]
    fn close_waits_for_an_admitted_send_before_releasing_provider_state() {
        let api = FakeApi::with_blocked_send();
        let shared = shared(api.clone(), ShutdownTiming::default());
        let sender = {
            let shared = shared.clone();
            thread::spawn(move || shared.send(&[IoSlice::new(b"reply")]).unwrap())
        };
        api.wait_for_send_entry();

        let closer = {
            let shared = shared.clone();
            thread::spawn(move || {
                shared
                    .close(Instant::now() + Duration::from_secs(1))
                    .unwrap()
            })
        };
        api.wait_for_event(Event::SessionExit);
        assert_eq!(lifecycle_events(&api.events()), [Event::SessionExit]);

        api.release_send();
        sender.join().unwrap();
        closer.join().unwrap();
        assert_eq!(
            lifecycle_events(&api.events()),
            [
                Event::SessionExit,
                Event::SessionRemoveChannel,
                Event::SessionDestroy,
                Event::DarwinUnmount,
                Event::ChannelDestroy,
            ]
        );
    }

    #[test]
    fn drain_timeout_retains_pointers_for_successful_retry() {
        let api = FakeApi::new(UnmountMode::Synchronous, ReceiveMode::IgnoreInterrupts);
        let shared = shared(
            api.clone(),
            ShutdownTiming {
                wake_interval: Duration::from_millis(5),
            },
        );
        let receiver = {
            let shared = shared.clone();
            thread::spawn(move || {
                let mut buffer = [0_u8; 64];
                assert_eq!(shared.receive(&mut buffer), Err(nix::errno::Errno::ENODEV));
            })
        };
        api.wait_for_receive_entry();

        let error = shared
            .close(Instant::now() + Duration::from_millis(30))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(api.channel_references(), 1);
        assert_eq!(
            lifecycle_events(&api.events()),
            [Event::SessionExit],
            "timeout must not detach, destroy, or unmount"
        );
        let events_before_late_calls = api.events();
        assert_eq!(
            shared.send(&[IoSlice::new(b"late")]).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        let mut buffer = [0_u8; 64];
        assert_eq!(shared.receive(&mut buffer), Err(nix::errno::Errno::ENODEV));
        assert_eq!(api.events(), events_before_late_calls);

        api.release_receive();
        receiver.join().unwrap();
        shared
            .close(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(api.channel_references(), 0);
        assert_eq!(
            lifecycle_events(&api.events()),
            [
                Event::SessionExit,
                Event::SessionRemoveChannel,
                Event::SessionDestroy,
                Event::DarwinUnmount,
                Event::ChannelDestroy,
            ]
        );
        assert_eq!(
            api.events()
                .iter()
                .filter(|event| **event == Event::SessionExit)
                .count(),
            1
        );
    }

    #[test]
    fn idle_sender_rejects_late_use_without_touching_provider() {
        let api = FakeApi::new(UnmountMode::Synchronous, ReceiveMode::Return);
        let shared = shared(api.clone(), ShutdownTiming::default());
        let sender = shared.clone();

        shared
            .close(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            sender.send(&[IoSlice::new(b"late")]).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        assert!(!api.events().contains(&Event::Send));
    }
}
