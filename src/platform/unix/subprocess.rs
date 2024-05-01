use crate::{duplex, entry, imp, Duplex, FnOnceObject, Object, Receiver, Serializer};
use nix::{
    libc::{c_char, pid_t},
    sched,
    sys::signal,
};
use std::ffi::{CStr, CString};
use std::io::Result;
use std::os::unix::io::{AsRawFd, RawFd};

/// The subprocess object created by calling `spawn` on a function annottated with `#[func]`.
pub struct Child<T: Object> {
    proc_pid: nix::unistd::Pid,
    output_rx: Receiver<T>,
}

impl<T: Object> Child<T> {
    pub(crate) fn new(proc_pid: nix::unistd::Pid, output_rx: Receiver<T>) -> Child<T> {
        Child {
            proc_pid,
            output_rx,
        }
    }

    /// Terminate the process immediately.
    pub fn kill(&mut self) -> Result<()> {
        signal::kill(self.proc_pid, signal::Signal::SIGKILL)?;
        Ok(())
    }

    /// Get ID of the process.
    pub fn id(&self) -> pid_t {
        self.proc_pid.as_raw()
    }

    /// Wait for the process to finish and obtain the value it returns.
    ///
    /// An error is returned if the process panics or is terminated. An error is also delivered if
    /// it exits via [`std::process::exit`] or alike instead of returning a value, unless the return
    /// type is `()`. In that case, `Ok(())` is returned.
    pub fn join(mut self) -> Result<T> {
        let mut value = self.output_rx.recv()?;
        if let Some(void) = imp::if_void::<T>() {
            // The value should be None at this moment
            value = Some(void);
        }
        let status = nix::sys::wait::waitpid(self.proc_pid, None)?;
        if let nix::sys::wait::WaitStatus::Exited(_, 0) = status {
            value.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "The subprocess terminated without returning a value",
                )
            })
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "The subprocess did not terminate successfully: {:?}",
                    status
                ),
            ))
        }
    }
}

pub(crate) unsafe fn _spawn_child<S: Object, R: Object>(
    child_fd: Duplex<S, R>,
    inherited_fds: &[RawFd],
) -> Result<nix::unistd::Pid> {
    let child_fd_str = CString::new(child_fd.as_raw_fd().to_string()).unwrap();

    let spawn_cb = || {
        // Use abort() instead of panic!() to prevent stack unwinding, as unwinding in the fork
        // child may free resources that would later be freed in the original process
        match fork_child_main(child_fd.as_raw_fd(), &child_fd_str, inherited_fds) {
            Ok(()) => unreachable!(),
            Err(e) => {
                eprintln!("{e}");
                std::process::abort();
            }
        }
    };

    let mut stack = [0u8; 4096];
    Ok(sched::clone(
        Box::new(spawn_cb),
        &mut stack,
        sched::CloneFlags::CLONE_VM | sched::CloneFlags::CLONE_VFORK,
        Some(nix::libc::SIGCHLD),
    )?)
}

unsafe fn fork_child_main(
    child_fd: RawFd,
    child_fd_str: &CStr,
    inherited_fds: &[RawFd],
) -> Result<()> {
    // No heap allocations are allowed here.
    for i in 1..32 {
        if i != nix::libc::SIGKILL && i != nix::libc::SIGSTOP {
            signal::sigaction(
                signal::Signal::try_from(i).unwrap(),
                &signal::SigAction::new(
                    signal::SigHandler::SigDfl,
                    signal::SaFlags::empty(),
                    signal::SigSet::empty(),
                ),
            )?;
        }
    }
    signal::sigprocmask(
        signal::SigmaskHow::SIG_SETMASK,
        Some(&signal::SigSet::empty()),
        None,
    )?;

    entry::disable_cloexec(child_fd)?;
    for fd in inherited_fds {
        entry::disable_cloexec(*fd)?;
    }

    // nix::unistd::execv uses allocations
    nix::libc::execv(
        b"/proc/self/exe\0" as *const u8 as *const c_char,
        &[
            b"_crossmist_\0" as *const u8 as *const c_char,
            child_fd_str.as_ptr() as *const u8 as *const c_char,
            std::ptr::null(),
        ] as *const *const c_char,
    );

    Err(std::io::Error::last_os_error())
}

pub unsafe fn spawn<T: Object>(
    entry: Box<dyn FnOnceObject<(RawFd,), Output = i32>>,
) -> Result<Child<T>> {
    imp::perform_sanity_checks();

    let mut s = Serializer::new();
    s.serialize(&entry);

    let fds = s.drain_handles();

    let (mut local, child) = duplex::<(Vec<u8>, Vec<RawFd>), T>()?;
    let pid = _spawn_child(child, &fds)?;
    local.send(&(s.into_vec(), fds))?;

    Ok(Child::new(pid, local.into_receiver()))
}
