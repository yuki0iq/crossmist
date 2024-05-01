use crate::{
    duplex, entry,
    handles::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    imp, FnOnceObject, Object, Receiver, Serializer,
};
use std::ffi::c_void;
use std::io::Result;
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation,
        System::{LibraryLoader, Threading, WindowsProgramming},
    },
};

/// The subprocess object created by calling `spawn` on a function annottated with `#[func]`.
pub struct Child<T: Object> {
    proc_handle: OwnedHandle,
    output_rx: Receiver<T>,
}

impl<T: Object> Child<T> {
    pub(crate) fn new(proc_handle: OwnedHandle, output_rx: Receiver<T>) -> Child<T> {
        Child {
            proc_handle,
            output_rx,
        }
    }

    /// Terminate the process immediately.
    pub fn kill(&mut self) -> Result<()> {
        unsafe {
            Threading::TerminateProcess(self.proc_handle.as_raw_handle(), 1).ok()?;
        }
        Ok(())
    }

    /// Get ID of the process.
    pub fn id(&self) -> RawHandle {
        self.proc_handle.as_raw_handle()
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
        if unsafe {
            Threading::WaitForSingleObject(
                self.proc_handle.as_raw_handle(),
                WindowsProgramming::INFINITE,
            )
        } == u32::MAX
        {
            return Err(std::io::Error::last_os_error());
        }
        let mut code: u32 = 0;
        unsafe {
            Threading::GetExitCodeProcess(self.proc_handle.as_raw_handle(), &mut code as *mut u32)
                .ok()?;
        }
        if code == 0 {
            value.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "The subprocess terminated without returning a value",
                )
            })
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("The subprocess terminated with exit code {code}"),
            ))
        }
    }
}

pub(crate) unsafe fn _spawn_child(
    child_tx: RawHandle,
    child_rx: RawHandle,
    inherited_handles: &[RawHandle],
) -> Result<OwnedHandle> {
    let mut inherited_handles = inherited_handles.to_vec();
    inherited_handles.push(child_tx);
    inherited_handles.push(child_rx);

    let handle_broker = *entry::HANDLE_BROKER
        .read()
        .expect("Failed to acquire read access to HANDLE_BROKER");
    if !handle_broker.is_invalid() {
        inherited_handles.push(handle_broker);
    }
    if let Some(sender) = entry::HANDLE_BROKER_HOLDER
        .read()
        .expect("Failed to acquire read access to HANDLE_BROKER_HOLDER")
        .as_ref()
    {
        inherited_handles.push(sender.as_raw_handle());
    }

    let mut module_name = vec![0u16; 256];
    let mut module_name_len;
    loop {
        module_name_len = LibraryLoader::GetModuleFileNameW(None, &mut module_name) as usize;
        if module_name_len == 0 {
            return Err(std::io::Error::last_os_error());
        } else if module_name_len == module_name.len() {
            module_name.resize(module_name.len() * 2, 0);
        } else {
            module_name.truncate(module_name_len + 1);
            break;
        }
    }

    let mut cmd_line: Vec<u16> = format!(
        "_crossmist_ {} {} {} {}\0",
        entry::HANDLE_BROKER
            .read()
            .expect("Failed to acquire read access to HANDLE_BROKER")
            .0,
        entry::HANDLE_BROKER_HOLDER
            .read()
            .expect("Failed to acquire read access to HANDLE_BROKER_HOLDER")
            .as_ref()
            .map(|sender| sender.as_raw_handle().0)
            .unwrap_or(0),
        child_tx.0,
        child_rx.0
    )
    .encode_utf16()
    .collect();

    let n_attrs = 1;
    let mut size = 0;
    Threading::InitializeProcThreadAttributeList(
        Threading::LPPROC_THREAD_ATTRIBUTE_LIST::default(),
        n_attrs,
        0,
        &mut size as *mut usize,
    );
    let mut attrs = vec![0u8; size];
    let attrs = Threading::LPPROC_THREAD_ATTRIBUTE_LIST(attrs.as_mut_ptr() as *mut c_void);
    Threading::InitializeProcThreadAttributeList(attrs, n_attrs, 0, &mut size as *mut usize)
        .ok()?;
    Threading::UpdateProcThreadAttribute(
        attrs,
        0,
        Threading::PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
        inherited_handles.as_ptr() as *const c_void,
        inherited_handles.len() * std::mem::size_of::<RawHandle>(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    )
    .ok()?;

    let mut startup_info = Threading::STARTUPINFOEXW::default();
    startup_info.StartupInfo.cb = std::mem::size_of::<Threading::STARTUPINFOEXW>() as u32;
    startup_info.lpAttributeList = attrs;

    let mut process_info = Threading::PROCESS_INFORMATION::default();

    let mut enabled_handles = Vec::new();
    for &handle in &inherited_handles {
        if entry::is_cloexec(handle)? {
            enabled_handles.push(handle);
            entry::disable_cloexec(handle)?;
        }
    }

    let res = Threading::CreateProcessW(
        PCWSTR::from_raw(module_name.as_ptr()),
        PWSTR::from_raw(cmd_line.as_mut_ptr()),
        std::ptr::null(),
        std::ptr::null(),
        true,
        Threading::EXTENDED_STARTUPINFO_PRESENT | Threading::INHERIT_PARENT_AFFINITY,
        std::ptr::null(),
        None,
        &startup_info as *const Threading::STARTUPINFOEXW as *const Threading::STARTUPINFOW,
        &mut process_info as *mut Threading::PROCESS_INFORMATION,
    );

    for handle in enabled_handles {
        entry::enable_cloexec(handle)?;
    }

    res.ok()?;

    Foundation::CloseHandle(process_info.hThread);
    Ok(OwnedHandle::from_raw_handle(process_info.hProcess))
}

pub unsafe fn spawn<T: Object>(
    entry: Box<dyn FnOnceObject<(RawHandle,), Output = i32>>,
) -> Result<Child<T>> {
    imp::perform_sanity_checks();

    let mut s = Serializer::new();
    s.serialize(&entry);

    let handles = s.drain_handles();

    let (mut local, child) = duplex::<(Vec<u8>, Vec<RawHandle>), T>()?;
    let handle = _spawn_child(
        child.0.sender.as_raw_handle(),
        child.0.receiver.as_raw_handle(),
        &handles,
    )?;
    local.send(&(s.into_vec(), handles))?;

    Ok(Child::new(handle, local.0.receiver.into()))
}
