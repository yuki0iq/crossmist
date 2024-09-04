//! Generic asynchronous implementation.
//!
//! This module contains generic definitions for functions using arbitrary asynchronous runtimes.
//! The [`crossmist::tokio`] and [`crossmist::smol`] modules provides type and functions definitions
//! for their respective runtimes. You should probably use those.
//!
//!
//! ## Channels
//!
//! Asynchronous channels work just like synchronous channels except that you need to add `.await`
//! to each blocking call. Synchronous and asynchronous channels can be converted to each other.
//! This might be useful if you use tokio/smol in the parent process but use synchronous code in the
//! child. In this case, you would create a channel using [`crossmist::channel`] and convert one
//! side to an asynchronous one.
//!
//!
//! ## Processes
//!
//! To start a child process, you use any of the `spawn_tokio` and `spawn_smol` methods generated by
//! `#[func]`:
//!
//! ```ignore
//! #[func]
//! fn my_process() {
//!     ...
//! }
//!
//! let child = my_process.spawn_tokio().await?;
//! // let child = my_process.spawn_smol().await?;
//! ```
//!
//! Note that you can use these methods on both synchronous and asynchronous functions, e.g. the
//! following works too:
//!
//! ```ignore
//! #[func(tokio)]
//! async fn my_process() {
//!     ...
//! }
//!
//! let child = my_process.spawn_tokio().await?;
//! ```

#[cfg(unix)]
use crate::internals::{socketpair, SingleObjectReceiver, SingleObjectSender};
use crate::{
    handles::{FromRawHandle, IntoRawHandle, RawHandle},
    imp, subprocess, FnOnceObject, Object, Serializer,
};
use std::fmt;
use std::future::Future;
use std::io::{Error, ErrorKind, Result};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use {
    crate::{
        handles::AsRawHandle,
        imp::implements,
        internals::{deserialize_with_handles, serialize_with_handles},
        pod::PlainOldData,
    },
    std::{mem::MaybeUninit, os::windows::io},
    windows::Win32::System::{Pipes, Threading, WindowsProgramming},
};

#[cfg(unix)]
pub(crate) type SyncStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub(crate) type SyncStream = std::fs::File;

/// Runtime-dependent stream implementation.
pub unsafe trait AsyncStream: Object + Sized {
    /// Create the stream from a sync stream.
    fn try_new(stream: SyncStream) -> Result<Self>;

    /// Get a raw handle to the underlying stream.
    fn as_raw_handle(&self) -> RawHandle;

    /// Whether socket operations should be blocking.
    #[cfg(unix)]
    const IS_BLOCKING: bool;

    /// Perform a blocking write.
    ///
    /// Calls `f`. If it returns `Err(WouldBlock)`, waits until the stream is writable and retries.
    /// When the function returns anything other than `Err(WouldBlock)`, returns.
    #[cfg(unix)]
    fn blocking_write<T>(
        &self,
        f: impl FnMut() -> Result<T> + Send,
    ) -> impl Future<Output = Result<T>> + Send;
    /// Perform a write.
    #[cfg(windows)]
    fn write(&mut self, buf: &[u8]) -> impl Future<Output = Result<()>> + Send;

    /// Perform a blocking read.
    ///
    /// Calls `f`. If it returns `Err(WouldBlock)`, waits until the stream is readable and retries.
    /// When the function returns anything other than `Err(WouldBlock)`, returns.
    #[cfg(unix)]
    fn blocking_read<T>(
        &self,
        f: impl FnMut() -> Result<T> + Send,
    ) -> impl Future<Output = Result<T>> + Send;
    /// Perform a read.
    #[cfg(windows)]
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = Result<()>> + Send;
}

/// The transmitting side of a unidirectional channel.
///
/// `T` is the type of the objects this side sends via the channel and the other side receives.
#[derive(Object)]
pub struct Sender<Stream: AsyncStream, T: Object> {
    pub(crate) fd: Stream,
    marker: PhantomData<fn(T)>,
}

/// The receiving side of a unidirectional channel.
///
/// `T` is the type of the objects the other side sends via the channel and this side receives.
#[derive(Object)]
pub struct Receiver<Stream: AsyncStream, T: Object> {
    pub(crate) fd: Stream,
    marker: PhantomData<fn() -> T>,
}

/// A side of a bidirectional channel.
///
/// `S` is the type of the objects this side sends via the channel and the other side receives, `R`
/// is the type of the objects the other side sends via the channel and this side receives.
#[derive(Object)]
pub struct Duplex<Stream: AsyncStream, S: Object, R: Object> {
    #[cfg(unix)]
    pub(crate) fd: Stream,
    #[cfg(unix)]
    marker: PhantomData<fn(S) -> R>,
    #[cfg(windows)]
    pub(crate) sender: Sender<Stream, S>,
    #[cfg(windows)]
    pub(crate) receiver: Receiver<Stream, R>,
}

/// Create a unidirectional channel.
pub fn channel<Stream: AsyncStream, T: Object>() -> Result<(Sender<Stream, T>, Receiver<Stream, T>)>
{
    #[cfg(unix)]
    {
        let (tx, rx) = duplex::<Stream, T, T>()?;
        Ok((tx.into_sender(), rx.into_receiver()))
    }
    #[cfg(windows)]
    {
        let mut tx: RawHandle = Default::default();
        let mut rx: RawHandle = Default::default();
        unsafe {
            Pipes::CreatePipe(
                &mut rx as *mut RawHandle,
                &mut tx as *mut RawHandle,
                std::ptr::null(),
                0,
            )
            .ok()?;
        }
        let tx = unsafe { SyncStream::from_raw_handle(tx) };
        let rx = unsafe { SyncStream::from_raw_handle(rx) };
        let tx = Sender {
            fd: Stream::try_new(tx)?,
            marker: PhantomData,
        };
        let rx = Receiver {
            fd: Stream::try_new(rx)?,
            marker: PhantomData,
        };
        Ok((tx, rx))
    }
}

/// Create a bidirectional channel.
pub fn duplex<Stream: AsyncStream, A: Object, B: Object>(
) -> Result<(Duplex<Stream, A, B>, Duplex<Stream, B, A>)> {
    #[cfg(unix)]
    {
        let (tx, rx) = socketpair()?;
        unsafe {
            Ok((
                Duplex::from_stream(Stream::try_new(tx)?),
                Duplex::from_stream(Stream::try_new(rx)?),
            ))
        }
    }
    #[cfg(windows)]
    {
        let (tx_a, rx_a) = channel::<Stream, A>()?;
        let (tx_b, rx_b) = channel::<Stream, B>()?;
        let ours = Duplex {
            sender: tx_a,
            receiver: rx_b,
        };
        let theirs = Duplex {
            sender: tx_b,
            receiver: rx_a,
        };
        Ok((ours, theirs))
    }
}

impl<Stream: AsyncStream, T: Object> Sender<Stream, T> {
    pub(crate) unsafe fn from_stream(fd: Stream) -> Self {
        Sender {
            fd,
            marker: PhantomData,
        }
    }

    /// Send a value to the other side.
    pub async fn send(&mut self, value: &T) -> Result<()> {
        #[cfg(unix)]
        {
            let mut sender =
                SingleObjectSender::new(self.fd.as_raw_handle(), value, Stream::IS_BLOCKING);
            self.fd.blocking_write(|| sender.send_next()).await
        }
        #[cfg(windows)]
        if implements!(T: PlainOldData) {
            let serialized = unsafe {
                std::slice::from_raw_parts(value as *const T as *const u8, std::mem::size_of::<T>())
            };
            self.fd.write(&serialized.len().to_ne_bytes()).await?;
            self.fd.write(serialized).await
        } else {
            let serialized = serialize_with_handles(value)?;
            self.fd.write(&serialized.len().to_ne_bytes()).await?;
            self.fd.write(&serialized).await
        }
    }
}

impl<Stream: AsyncStream + fmt::Debug, T: Object> fmt::Debug for Sender<Stream, T> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_tuple("Sender").field(&self.fd).finish()
    }
}

impl<Stream: AsyncStream, T: Object> TryFrom<crate::Sender<T>> for Sender<Stream, T> {
    type Error = Error;
    fn try_from(value: crate::Sender<T>) -> Result<Self> {
        unsafe {
            Ok(Self::from_stream(Stream::try_new(
                SyncStream::from_raw_handle(value.into_raw_handle()),
            )?))
        }
    }
}

#[cfg(unix)]
impl<Stream: AsyncStream, T: Object> std::os::unix::io::AsRawFd for Sender<Stream, T> {
    fn as_raw_fd(&self) -> RawHandle {
        self.fd.as_raw_handle()
    }
}
#[cfg(windows)]
impl<Stream: AsyncStream, T: Object> io::AsRawHandle for Sender<Stream, T> {
    fn as_raw_handle(&self) -> std::os::windows::prelude::RawHandle {
        self.fd.as_raw_handle().0 as _
    }
}

impl<Stream: AsyncStream, T: Object> Receiver<Stream, T> {
    pub(crate) unsafe fn from_stream(fd: Stream) -> Self {
        Receiver {
            fd,
            marker: PhantomData,
        }
    }

    /// Receive a value from the other side.
    ///
    /// Returns `Ok(None)` if the other side has dropped the channel.
    pub async fn recv(&mut self) -> Result<Option<T>> {
        #[cfg(unix)]
        {
            let mut receiver =
                unsafe { SingleObjectReceiver::new(self.fd.as_raw_handle(), Stream::IS_BLOCKING) };
            self.fd.blocking_read(|| receiver.recv_next()).await
        }
        #[cfg(windows)]
        {
            let mut len = [0u8; std::mem::size_of::<usize>()];
            if let Err(e) = self.fd.read(&mut len).await {
                if e.kind() == ErrorKind::UnexpectedEof {
                    return Ok(None);
                }
                return Err(e);
            }
            let len = usize::from_ne_bytes(len);

            if implements!(T: PlainOldData) {
                struct Wrapper<T>(MaybeUninit<T>);
                unsafe impl<T> Send for Wrapper<T> {}
                let mut serialized = Wrapper::<T>(MaybeUninit::zeroed());
                self.fd
                    .read(unsafe {
                        std::slice::from_raw_parts_mut(
                            serialized.0.as_mut_ptr() as *mut u8,
                            std::mem::size_of::<T>(),
                        )
                    })
                    .await?;
                Ok(Some(unsafe { serialized.0.assume_init() }))
            } else {
                let mut serialized = vec![0u8; len];
                self.fd.read(&mut serialized).await?;
                unsafe { deserialize_with_handles(serialized).map(Some) }
            }
        }
    }
}

impl<Stream: AsyncStream + fmt::Debug, T: Object> fmt::Debug for Receiver<Stream, T> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_tuple("Receiver").field(&self.fd).finish()
    }
}

impl<Stream: AsyncStream, T: Object> TryFrom<crate::Receiver<T>> for Receiver<Stream, T> {
    type Error = Error;
    fn try_from(value: crate::Receiver<T>) -> Result<Self> {
        unsafe {
            Ok(Self::from_stream(Stream::try_new(
                SyncStream::from_raw_handle(value.into_raw_handle()),
            )?))
        }
    }
}

#[cfg(unix)]
impl<Stream: AsyncStream, T: Object> std::os::unix::io::AsRawFd for Receiver<Stream, T> {
    fn as_raw_fd(&self) -> RawHandle {
        self.fd.as_raw_handle()
    }
}
#[cfg(windows)]
impl<Stream: AsyncStream, T: Object> io::AsRawHandle for Receiver<Stream, T> {
    fn as_raw_handle(&self) -> std::os::windows::prelude::RawHandle {
        self.fd.as_raw_handle().0 as _
    }
}

impl<Stream: AsyncStream, S: Object, R: Object> Duplex<Stream, S, R> {
    #[cfg(unix)]
    pub(crate) unsafe fn from_stream(fd: Stream) -> Self {
        Duplex {
            fd,
            marker: PhantomData,
        }
    }

    /// Send a value to the other side.
    pub async fn send(&mut self, value: &S) -> Result<()> {
        #[cfg(unix)]
        {
            let mut sender =
                SingleObjectSender::new(self.fd.as_raw_handle(), value, Stream::IS_BLOCKING);
            self.fd.blocking_write(|| sender.send_next()).await
        }
        #[cfg(windows)]
        self.sender.send(value).await
    }

    /// Receive a value from the other side.
    ///
    /// Returns `Ok(None)` if the other side has dropped the channel.
    pub async fn recv(&mut self) -> Result<Option<R>> {
        #[cfg(unix)]
        {
            let mut receiver =
                unsafe { SingleObjectReceiver::new(self.fd.as_raw_handle(), Stream::IS_BLOCKING) };
            self.fd.blocking_read(|| receiver.recv_next()).await
        }
        #[cfg(windows)]
        self.receiver.recv().await
    }

    /// Send a value from the other side and wait for a response immediately.
    ///
    /// If the other side closes the channel before responding, an error is returned.
    pub async fn request(&mut self, value: &S) -> Result<R> {
        self.send(value).await?;
        self.recv().await?.ok_or_else(|| {
            Error::new(
                ErrorKind::UnexpectedEof,
                "The subprocess exitted before responding to the request",
            )
        })
    }

    pub fn into_sender(self) -> Sender<Stream, S> {
        #[cfg(unix)]
        unsafe {
            Sender::from_stream(self.fd)
        }
        #[cfg(windows)]
        self.sender
    }

    pub fn into_receiver(self) -> Receiver<Stream, R> {
        #[cfg(unix)]
        unsafe {
            Receiver::from_stream(self.fd)
        }
        #[cfg(windows)]
        self.receiver
    }
}

impl<Stream: AsyncStream + fmt::Debug, S: Object, R: Object> fmt::Debug for Duplex<Stream, S, R> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        #[cfg(unix)]
        {
            fmt.debug_tuple("Duplex").field(&self.fd).finish()
        }
        #[cfg(windows)]
        {
            fmt.debug_struct("Duplex")
                .field("sender", &self.sender)
                .field("receiver", &self.receiver)
                .finish()
        }
    }
}

impl<Stream: AsyncStream, S: Object, R: Object> TryFrom<crate::Duplex<S, R>>
    for Duplex<Stream, S, R>
{
    type Error = Error;
    fn try_from(value: crate::Duplex<S, R>) -> Result<Self> {
        #[cfg(unix)]
        unsafe {
            Ok(Self::from_stream(Stream::try_new(
                SyncStream::from_raw_handle(value.into_raw_handle()),
            )?))
        }
        #[cfg(windows)]
        {
            Ok(Self {
                sender: crate::Sender(value.0.sender).try_into()?,
                receiver: crate::Receiver(value.0.receiver).try_into()?,
            })
        }
    }
}

#[cfg(unix)]
impl<Stream: AsyncStream, S: Object, R: Object> std::os::unix::io::AsRawFd
    for Duplex<Stream, S, R>
{
    fn as_raw_fd(&self) -> RawHandle {
        self.fd.as_raw_handle()
    }
}

#[cfg(unix)]
type ProcHandle = rustix::process::Pid;
#[cfg(windows)]
type ProcHandle = crate::handles::OwnedHandle;

#[cfg(unix)]
pub(crate) type ProcID = rustix::process::RawPid;
#[cfg(windows)]
pub(crate) type ProcID = RawHandle;

/// A subprocess.
pub struct Child<Stream: AsyncStream, T: Object> {
    proc_handle: ProcHandle,
    output_rx: Receiver<Stream, T>,
    may_kill: Arc<Mutex<bool>>,
}

/// A handle that allows to kill the process.
pub struct KillHandle {
    proc_id: ProcID,
    may_kill: Arc<Mutex<bool>>,
}

impl<Stream: AsyncStream, T: Object> Child<Stream, T> {
    fn new(proc_handle: ProcHandle, output_rx: Receiver<Stream, T>) -> Child<Stream, T> {
        Child {
            proc_handle,
            output_rx,
            may_kill: Arc::new(Mutex::new(true)),
        }
    }

    /// Get a handle for process termination.
    pub fn get_kill_handle(&self) -> crate::KillHandle {
        KillHandle {
            proc_id: self.id(),
            may_kill: self.may_kill.clone(),
        }
    }

    /// Get ID of the process.
    pub fn id(&self) -> ProcID {
        #[cfg(unix)]
        {
            rustix::process::Pid::as_raw(Some(self.proc_handle))
        }
        #[cfg(windows)]
        {
            self.proc_handle.as_raw_handle()
        }
    }

    /// Wait for the process to finish and obtain the value it returns.
    ///
    /// An error is returned if the process panics or is terminated. An error is also delivered if
    /// it exits via [`std::process::exit`] or alike instead of returning a value, unless the return
    /// type is `()`. In that case, `Ok(())` is returned.
    pub async fn join(mut self) -> Result<T> {
        let mut value = self.output_rx.recv().await?;
        if let Some(void) = imp::if_void::<T>() {
            // The value should be None at this moment
            value = Some(void);
        }
        let mut guard = self.may_kill.lock().expect("Kill mutex is poisoned");
        *guard = false;
        // This is synchronous, but should be really fast
        #[cfg(unix)]
        {
            let status = rustix::process::waitpid(
                Some(self.proc_handle),
                rustix::process::WaitOptions::empty(),
            )?
            .unwrap();
            if status.exit_status() == Some(0) {
                value.ok_or_else(|| {
                    Error::new(
                        ErrorKind::Other,
                        "The subprocess terminated without returning a value",
                    )
                })
            } else {
                Err(Error::new(
                    ErrorKind::Other,
                    format!(
                        "The subprocess did not terminate successfully: {:?}",
                        status
                    ),
                ))
            }
        }
        #[cfg(windows)]
        {
            if unsafe {
                Threading::WaitForSingleObject(
                    self.proc_handle.as_raw_handle(),
                    WindowsProgramming::INFINITE,
                )
            } == u32::MAX
            {
                return Err(Error::last_os_error());
            }
            let mut code: u32 = 0;
            unsafe {
                Threading::GetExitCodeProcess(
                    self.proc_handle.as_raw_handle(),
                    &mut code as *mut u32,
                )
                .ok()?;
            }
            if code == 0 {
                value.ok_or_else(|| {
                    Error::new(
                        ErrorKind::Other,
                        "The subprocess terminated without returning a value",
                    )
                })
            } else {
                Err(Error::new(
                    ErrorKind::Other,
                    format!("The subprocess terminated with exit code {code}"),
                ))
            }
        }
    }
}

impl<Stream: AsyncStream + fmt::Debug, T: Object> fmt::Debug for Child<Stream, T> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Child")
            .field("proc_handle", &self.proc_handle)
            .field("output_rx", &self.output_rx)
            .finish()
    }
}

impl KillHandle {
    /// Terminate the process immediately.
    pub fn kill(&self) -> Result<()> {
        let guard = self.may_kill.lock().expect("Kill mutex is poisoned");
        if !*guard {
            return Err(std::io::Error::other(
                "This process has already been joined",
            ));
        }
        #[cfg(unix)]
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(self.proc_id).unwrap(),
            rustix::process::Signal::Kill,
        )?;
        #[cfg(windows)]
        unsafe {
            Threading::TerminateProcess(self.proc_id, 1).ok()?;
        }
        Ok(())
    }
}

impl fmt::Debug for KillHandle {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("KillHandle")
            .field("proc_id", &self.proc_id)
            .finish()
    }
}

pub(crate) async unsafe fn spawn<Stream: AsyncStream, T: Object>(
    entry: Box<dyn FnOnceObject<(RawHandle,), Output = i32>>,
) -> Result<Child<Stream, T>> {
    imp::perform_sanity_checks();

    let mut s = Serializer::new();
    s.serialize(&entry);

    let handles = s.drain_handles();

    let (local, child) = crate::duplex()?;
    let mut local: Duplex<Stream, (Vec<u8>, Vec<RawHandle>), T> = local.try_into()?;

    let process_handle;
    let receiver;

    #[cfg(unix)]
    {
        process_handle = subprocess::_spawn_child(child, &handles)?;
        local.send(&(s.into_vec(), handles)).await?;
        receiver = Receiver::from_stream(local.fd);
    }

    #[cfg(windows)]
    {
        process_handle = subprocess::_spawn_child(
            child.0.sender.as_raw_handle(),
            child.0.receiver.as_raw_handle(),
            &handles,
        )?;
        local.send(&(s.into_vec(), handles)).await?;
        receiver = local.receiver;
    }

    Ok(Child::new(process_handle, receiver))
}
