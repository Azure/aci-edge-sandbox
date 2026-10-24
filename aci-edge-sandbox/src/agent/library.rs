// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Checked binding of a separately supplied library's sandbox ABI, loaded once per process.

#[cfg(not(target_pointer_width = "64"))]
compile_error!("the sandbox ABI is supported only on 64-bit hosts");

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::ptr;
use std::slice;
use std::sync::{Mutex, OnceLock, PoisonError};

use aci_edge_sandboxes_model::wire::SANDBOX_ABI_VERSION;
use libloading::Library as Loaded;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use crate::error::{Error, ErrorBody, Result};

const STATUS_OK: i32 = 0;
const STATUS_ERROR: i32 = 1;
const STATUS_TIMEOUT: i32 = 2;
const STATUS_INVALID_HANDLE: i32 = -1;
const STATUS_INVALID_ARGUMENT: i32 = -2;
const STATUS_PANIC: i32 = -3;
const EVENT_STDOUT: u32 = 1;
const EVENT_STDERR: u32 = 2;
const EVENT_EXIT: u32 = 3;

#[repr(C)]
struct ExecEvent {
    kind: u32,
    data: *const u8,
    length: usize,
}

const _: () = assert!(size_of::<ExecEvent>() == 24);

type Version = unsafe extern "C" fn() -> u32;
type HostOpen = unsafe extern "C" fn(*const u8, usize, *mut u64, *mut *mut u8, *mut usize) -> i32;
type Release = unsafe extern "C" fn(u64) -> i32;
type Query = unsafe extern "C" fn(u64, *mut *mut u8, *mut usize) -> i32;
type Call = unsafe extern "C" fn(u64, *const u8, usize, *mut *mut u8, *mut usize) -> i32;
type Exec = unsafe extern "C" fn(
    u64,
    *const u8,
    usize,
    *const u8,
    usize,
    *mut u64,
    *mut *mut u8,
    *mut usize,
) -> i32;
type ExecNext = unsafe extern "C" fn(u64, i64, *mut ExecEvent) -> i32;
type ExecWrite =
    unsafe extern "C" fn(u64, *const u8, usize, i64, *mut usize, *mut *mut u8, *mut usize) -> i32;
type BufferFree = unsafe extern "C" fn(*mut u8, usize);

/// The exports of the sandbox ABI.
struct Exports {
    host_open: HostOpen,
    host_close: Release,
    host_info: Query,
    capabilities: Query,
    probe: Query,
    validate_provision: Call,
    validate_exec: Call,
    provision: Call,
    start: Call,
    stop: Call,
    deprovision: Call,
    diagnostics: Call,
    guest_logs: Call,
    exec: Exec,
    exec_next: ExecNext,
    exec_write: ExecWrite,
    exec_close_input: Query,
    exec_cancel: Query,
    exec_release: Release,
    image_register: Call,
    image_list: Query,
    image_verify: Call,
    image_unregister: Call,
    buffer_free: BufferFree,
}

/// A loaded, verified library. Libraries stay loaded until the process exits, so the state that
/// a library keeps for the process outlives every backend.
pub(crate) struct Library {
    exports: Exports,
    /// Keeps the exports' code mapped; a test double binds exports without a file.
    _library: Option<Loaded>,
    /// The descriptor that the library was loaded through, if any. It stays open so that its
    /// path never names another library.
    _image: Option<File>,
}

impl std::fmt::Debug for Library {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Library").finish_non_exhaustive()
    }
}

/// One event of an execution, borrowed from the library until the next event.
pub(crate) enum Event<'a> {
    Stdout(&'a [u8]),
    Stderr(&'a [u8]),
    /// How the execution ended, as JSON.
    Exit(&'a [u8]),
}

fn resolve<T: Copy>(library: &Loaded, name: &[u8]) -> Result<T> {
    // Function pointers stay valid because the library is never unloaded.
    unsafe { library.get::<T>(name) }
        .map(|symbol| *symbol)
        .map_err(|error| {
            Error::backend_unavailable(format!(
                "the agent library is missing required export {}",
                String::from_utf8_lossy(name).trim_end_matches('\0')
            ))
            .with_source(error)
        })
}

/// Opens the library to check it. On Windows, the handle keeps writers out of the file and keeps
/// the file and its directories from being renamed or deleted while it is open.
fn open_for_load(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::share_mode(
        &mut options,
        windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ,
    );
    options.open(path)
}

/// Loads the checked `bytes` from a sealed in-memory copy, so that nothing resolves `path` again
/// and no writer of the file can change what loads. A kernel that refuses to create or execute
/// such a copy fails the load. Returns the copy, through which the library was loaded.
#[cfg(target_os = "linux")]
fn load_checked(path: &Path, file: File, bytes: &[u8]) -> Result<(Loaded, Option<File>)> {
    drop(file);
    let image = sealed_copy(bytes).map_err(|error| {
        Error::backend_unavailable(format!(
            "cannot copy the agent library at {} into a sealed in-memory file",
            path.display()
        ))
        .with_source(error)
    })?;
    let library =
        unsafe { Loaded::new(descriptor_path(&image)) }.map_err(|error| unloadable(path, error))?;
    Ok((library, Some(image)))
}

/// Loads the library at `path` while `file`, opened by [`open_for_load`], keeps the checked file
/// and its directories in place.
#[cfg(windows)]
fn load_checked(path: &Path, file: File, _bytes: &[u8]) -> Result<(Loaded, Option<File>)> {
    let library = unsafe { Loaded::new(path) }.map_err(|error| unloadable(path, error))?;
    drop(file);
    Ok((library, None))
}

/// Refuses to load the library: on other hosts, nothing keeps the checked bytes from changing
/// before they load.
#[cfg(not(any(windows, target_os = "linux")))]
fn load_checked(path: &Path, _file: File, _bytes: &[u8]) -> Result<(Loaded, Option<File>)> {
    Err(Error::backend_unavailable(format!(
        "cannot load the agent library at {}: only Windows and Linux hosts can load it",
        path.display()
    )))
}

#[cfg(any(windows, target_os = "linux"))]
fn unloadable(path: &Path, error: libloading::Error) -> Error {
    Error::backend_unavailable(format!(
        "cannot load the agent library from {}",
        path.display()
    ))
    .with_source(error)
}

/// The path through which this process opens `file` again.
#[cfg(target_os = "linux")]
fn descriptor_path(file: &File) -> String {
    use std::os::fd::AsRawFd;
    format!("/proc/self/fd/{}", file.as_raw_fd())
}

/// Copies `bytes` into an in-memory file and seals it against every change.
#[cfg(target_os = "linux")]
fn sealed_copy(bytes: &[u8]) -> io::Result<File> {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let name = c"aci_edge_agent";
    let flags = libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING;
    // `MFD_EXEC` makes the copy executable where `vm.memfd_noexec` is 1. Kernels before 6.3 reject
    // the flag, and their copies are executable without it.
    // SAFETY: `name` is a C string.
    let mut fd = unsafe { libc::memfd_create(name.as_ptr(), flags | libc::MFD_EXEC) };
    if fd < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
        // SAFETY: as above.
        fd = unsafe { libc::memfd_create(name.as_ptr(), flags) };
    }
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `memfd_create` returned a new descriptor that nothing else owns.
    let mut file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    file.write_all(bytes)?;
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    // SAFETY: the descriptor is open, and `F_ADD_SEALS` takes an integer.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

/// Returns the library at the absolute `path`, loading it if this process has not, after
/// checking that it has the approved SHA-256 and the sandbox ABI that this crate speaks. The
/// bytes that it loads are the bytes that it checked, so a writer cannot swap the file in between;
/// only Windows and Linux hosts can load a library.
///
/// A loaded library stays loaded, under its canonical path and every path that it was requested
/// by, so a later request by the same path touches no file.
pub(crate) fn load(path: &Path, expected_sha256: &[u8; 32]) -> Result<&'static Library> {
    type Loads = Mutex<HashMap<(PathBuf, [u8; 32]), &'static Library>>;
    static LOADED: OnceLock<Loads> = OnceLock::new();
    if !path.is_absolute() {
        return Err(Error::backend_unavailable(
            "the agent library path must be absolute",
        ));
    }
    let loads = LOADED.get_or_init(Default::default);
    let requested = (path.to_path_buf(), *expected_sha256);
    if let Some(library) = loads
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&requested)
    {
        return Ok(library);
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        Error::backend_unavailable(format!(
            "cannot locate the agent library at {}",
            path.display()
        ))
        .with_source(error)
    })?;
    let mut loaded = loads.lock().unwrap_or_else(PoisonError::into_inner);
    let key = (canonical, *expected_sha256);
    let library = match loaded.get(&key) {
        Some(library) => *library,
        None => {
            let library: &'static Library =
                Box::leak(Box::new(Library::open(&key.0, expected_sha256)?));
            loaded.insert(key, library);
            library
        }
    };
    loaded.insert(requested, library);
    Ok(library)
}

fn error_from_body(bytes: Option<&[u8]>) -> Error {
    match bytes.map(serde_json::from_slice::<ErrorBody>) {
        Some(Ok(body)) => Error::new(body.code, body.message),
        _ => {
            Error::backend_error("the agent library reported an error without a valid description")
        }
    }
}

fn status_error(status: i32) -> Error {
    Error::backend_error(match status {
        STATUS_INVALID_HANDLE => {
            "the agent library rejected a released or unknown handle".to_owned()
        }
        STATUS_INVALID_ARGUMENT => "the agent library rejected an argument".to_owned(),
        STATUS_PANIC => "the agent library panicked; the operation's effect is unknown".to_owned(),
        other => format!("the agent library returned the unexpected status {other}"),
    })
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec(value)
        .map_err(|error| Error::malformed_request("cannot encode the request").with_source(error))
}

fn decode<T: DeserializeOwned>(bytes: Option<Vec<u8>>, what: &str) -> Result<T> {
    let bytes = bytes
        .ok_or_else(|| Error::backend_error(format!("the agent library returned no {what}")))?;
    serde_json::from_slice(&bytes).map_err(|error| {
        Error::backend_error(format!("the agent library returned a malformed {what}"))
            .with_source(error)
    })
}

impl Library {
    fn open(path: &Path, expected_sha256: &[u8; 32]) -> Result<Self> {
        let unverifiable = |error: io::Error| {
            Error::backend_unavailable(format!(
                "cannot verify the agent library at {}",
                path.display()
            ))
            .with_source(error)
        };
        let mut file = open_for_load(path).map_err(unverifiable)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(unverifiable)?;
        let actual: [u8; 32] = Sha256::digest(&bytes).into();
        if &actual != expected_sha256 {
            return Err(Error::backend_unavailable(format!(
                "the agent library at {} does not match the approved SHA-256",
                path.display()
            )));
        }
        let (library, image) = load_checked(path, file, &bytes)?;
        let version: Version = resolve(&library, b"aci_edge_sandbox_api_version\0")?;
        let actual_version = unsafe { version() };
        if actual_version != SANDBOX_ABI_VERSION {
            return Err(Error::backend_unavailable(format!(
                "the agent library's sandbox ABI is {actual_version}, expected {SANDBOX_ABI_VERSION}"
            )));
        }
        let exports = Exports {
            host_open: resolve(&library, b"aci_edge_sandbox_host_open\0")?,
            host_close: resolve(&library, b"aci_edge_sandbox_host_close\0")?,
            host_info: resolve(&library, b"aci_edge_sandbox_host_info\0")?,
            capabilities: resolve(&library, b"aci_edge_sandbox_capabilities\0")?,
            probe: resolve(&library, b"aci_edge_sandbox_probe\0")?,
            validate_provision: resolve(&library, b"aci_edge_sandbox_validate_provision\0")?,
            validate_exec: resolve(&library, b"aci_edge_sandbox_validate_exec\0")?,
            provision: resolve(&library, b"aci_edge_sandbox_provision\0")?,
            start: resolve(&library, b"aci_edge_sandbox_start\0")?,
            stop: resolve(&library, b"aci_edge_sandbox_stop\0")?,
            deprovision: resolve(&library, b"aci_edge_sandbox_deprovision\0")?,
            diagnostics: resolve(&library, b"aci_edge_sandbox_diagnostics\0")?,
            guest_logs: resolve(&library, b"aci_edge_sandbox_guest_logs\0")?,
            exec: resolve(&library, b"aci_edge_sandbox_exec\0")?,
            exec_next: resolve(&library, b"aci_edge_exec_next\0")?,
            exec_write: resolve(&library, b"aci_edge_exec_write\0")?,
            exec_close_input: resolve(&library, b"aci_edge_exec_close_input\0")?,
            exec_cancel: resolve(&library, b"aci_edge_exec_cancel\0")?,
            exec_release: resolve(&library, b"aci_edge_exec_release\0")?,
            image_register: resolve(&library, b"aci_edge_image_register\0")?,
            image_list: resolve(&library, b"aci_edge_image_list\0")?,
            image_verify: resolve(&library, b"aci_edge_image_verify\0")?,
            image_unregister: resolve(&library, b"aci_edge_image_unregister\0")?,
            buffer_free: resolve(&library, b"aci_edge_buffer_free\0")?,
        };
        Ok(Self {
            exports,
            _library: Some(library),
            _image: image,
        })
    }

    /// Copies and frees a buffer that the library returned.
    fn take(&self, buffer: *mut u8, length: usize) -> Option<Vec<u8>> {
        if buffer.is_null() {
            return None;
        }
        // SAFETY: the library returned this buffer and length, and nothing else frees it.
        let bytes = unsafe { slice::from_raw_parts(buffer, length) }.to_vec();
        unsafe { (self.exports.buffer_free)(buffer, length) };
        Some(bytes)
    }

    /// Runs an export that answers through an out buffer, returning the result's bytes.
    fn call(
        &self,
        export: impl FnOnce(*mut *mut u8, *mut usize) -> i32,
    ) -> Result<Option<Vec<u8>>> {
        let mut buffer = ptr::null_mut();
        let mut length = 0;
        let status = export(&mut buffer, &mut length);
        let bytes = self.take(buffer, length);
        match status {
            STATUS_OK => Ok(bytes),
            STATUS_ERROR => Err(error_from_body(bytes.as_deref())),
            other => Err(status_error(other)),
        }
    }

    fn query(&self, export: Query, handle: u64) -> Result<Option<Vec<u8>>> {
        self.call(|out, length| unsafe { export(handle, out, length) })
    }

    fn with(&self, export: Call, host: u64, input: &[u8]) -> Result<Option<Vec<u8>>> {
        self.call(|out, length| unsafe { export(host, input.as_ptr(), input.len(), out, length) })
    }

    /// Opens the host of `setup` and returns its handle.
    pub(crate) fn host_open(&self, setup: &impl Serialize) -> Result<u64> {
        let setup = encode(setup)?;
        let mut host = 0;
        self.call(|out, length| unsafe {
            (self.exports.host_open)(setup.as_ptr(), setup.len(), &mut host, out, length)
        })?;
        Ok(host)
    }

    pub(crate) fn host_close(&self, host: u64) {
        unsafe { (self.exports.host_close)(host) };
    }

    pub(crate) fn host_info<T: DeserializeOwned>(&self, host: u64) -> Result<T> {
        decode(
            self.query(self.exports.host_info, host)?,
            "host description",
        )
    }

    pub(crate) fn capabilities<T: DeserializeOwned>(&self, host: u64) -> Result<T> {
        decode(
            self.query(self.exports.capabilities, host)?,
            "capability set",
        )
    }

    pub(crate) fn probe(&self, host: u64) -> Result<()> {
        self.query(self.exports.probe, host).map(drop)
    }

    pub(crate) fn validate_provision(&self, host: u64, input: &impl Serialize) -> Result<()> {
        self.with(self.exports.validate_provision, host, &encode(input)?)
            .map(drop)
    }

    pub(crate) fn validate_exec(&self, host: u64, request: &impl Serialize) -> Result<()> {
        self.with(self.exports.validate_exec, host, &encode(request)?)
            .map(drop)
    }

    pub(crate) fn provision<T: DeserializeOwned>(
        &self,
        host: u64,
        input: &impl Serialize,
    ) -> Result<T> {
        let reply = self.with(self.exports.provision, host, &encode(input)?)?;
        decode(reply, "provision result")
    }

    /// Runs a lifecycle operation on the sandbox `token`.
    fn sandbox<T: DeserializeOwned>(
        &self,
        export: Call,
        host: u64,
        token: &str,
        what: &str,
    ) -> Result<T> {
        decode(self.with(export, host, token.as_bytes())?, what)
    }

    pub(crate) fn start<T: DeserializeOwned>(&self, host: u64, token: &str) -> Result<T> {
        self.sandbox(self.exports.start, host, token, "start result")
    }

    pub(crate) fn stop<T: DeserializeOwned>(&self, host: u64, token: &str) -> Result<T> {
        self.sandbox(self.exports.stop, host, token, "stop result")
    }

    pub(crate) fn deprovision<T: DeserializeOwned>(&self, host: u64, token: &str) -> Result<T> {
        self.sandbox(self.exports.deprovision, host, token, "deprovision result")
    }

    pub(crate) fn diagnostics<T: DeserializeOwned>(&self, host: u64, token: &str) -> Result<T> {
        self.sandbox(self.exports.diagnostics, host, token, "diagnostics")
    }

    pub(crate) fn guest_logs(&self, host: u64, token: &str) -> Result<Vec<u8>> {
        Ok(self
            .with(self.exports.guest_logs, host, token.as_bytes())?
            .unwrap_or_default())
    }

    /// Starts a command and returns its execution handle.
    pub(crate) fn exec(&self, host: u64, token: &str, input: &impl Serialize) -> Result<u64> {
        let input = encode(input)?;
        let mut exec = 0;
        self.call(|out, length| unsafe {
            (self.exports.exec)(
                host,
                token.as_ptr(),
                token.len(),
                input.as_ptr(),
                input.len(),
                &mut exec,
                out,
                length,
            )
        })?;
        Ok(exec)
    }

    /// Waits for an execution's next event and passes it to `deliver`; `None` waits
    /// indefinitely. Returns `None` when no event arrived in time.
    pub(crate) fn exec_next<T>(
        &self,
        exec: u64,
        timeout_ms: Option<u64>,
        deliver: impl FnOnce(Event<'_>) -> T,
    ) -> Result<Option<T>> {
        let mut event = ExecEvent {
            kind: 0,
            data: ptr::null(),
            length: 0,
        };
        let timeout = timeout_ms.map_or(-1, |timeout| i64::try_from(timeout).unwrap_or(i64::MAX));
        match unsafe { (self.exports.exec_next)(exec, timeout, &mut event) } {
            STATUS_OK => {}
            STATUS_TIMEOUT => return Ok(None),
            other => return Err(status_error(other)),
        }
        let data = if event.length == 0 {
            &[][..]
        } else if event.data.is_null() {
            return Err(Error::backend_error(
                "the agent library returned an execution event whose data is missing",
            ));
        } else {
            // SAFETY: the library keeps the event's bytes until the next call for this execution.
            unsafe { slice::from_raw_parts(event.data, event.length) }
        };
        match event.kind {
            EVENT_STDOUT => Ok(Some(deliver(Event::Stdout(data)))),
            EVENT_STDERR => Ok(Some(deliver(Event::Stderr(data)))),
            EVENT_EXIT => Ok(Some(deliver(Event::Exit(data)))),
            other => Err(Error::backend_error(format!(
                "the agent library returned the unknown execution event {other}"
            ))),
        }
    }

    /// Writes up to one frame of standard input, returning how many bytes the guest accepted, or
    /// 0 when it accepted nothing before `timeout_ms`.
    pub(crate) fn exec_write(
        &self,
        exec: u64,
        data: &[u8],
        timeout_ms: Option<u64>,
    ) -> Result<usize> {
        let timeout = timeout_ms.map_or(-1, |timeout| i64::try_from(timeout).unwrap_or(i64::MAX));
        let mut written = 0;
        let mut buffer = ptr::null_mut();
        let mut length = 0;
        let status = unsafe {
            (self.exports.exec_write)(
                exec,
                data.as_ptr(),
                data.len(),
                timeout,
                &mut written,
                &mut buffer,
                &mut length,
            )
        };
        let bytes = self.take(buffer, length);
        match status {
            STATUS_OK => Ok(written),
            STATUS_TIMEOUT => Ok(0),
            STATUS_ERROR => Err(error_from_body(bytes.as_deref())),
            other => Err(status_error(other)),
        }
    }

    pub(crate) fn exec_close_input(&self, exec: u64) -> Result<()> {
        self.query(self.exports.exec_close_input, exec).map(drop)
    }

    pub(crate) fn exec_cancel(&self, exec: u64) -> Result<()> {
        self.query(self.exports.exec_cancel, exec).map(drop)
    }

    pub(crate) fn exec_release(&self, exec: u64) {
        unsafe { (self.exports.exec_release)(exec) };
    }

    pub(crate) fn image_register<T: DeserializeOwned>(
        &self,
        host: u64,
        registration: &impl Serialize,
    ) -> Result<T> {
        let registered = self.with(self.exports.image_register, host, &encode(registration)?)?;
        decode(registered, "image registration")
    }

    pub(crate) fn image_list<T: DeserializeOwned>(&self, host: u64) -> Result<T> {
        decode(self.query(self.exports.image_list, host)?, "image list")
    }

    pub(crate) fn image_verify(&self, host: u64, id: &str) -> Result<()> {
        self.with(self.exports.image_verify, host, id.as_bytes())
            .map(drop)
    }

    pub(crate) fn image_unregister(&self, host: u64, id: &str) -> Result<bool> {
        decode(
            self.with(self.exports.image_unregister, host, id.as_bytes())?,
            "unregistration result",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;

    /// Binds the library that `EDGE_AGENT_TEST_LIBRARY` names as this crate does: the sandbox ABI
    /// version and every export, errors described in library buffers, and refused handles. It
    /// needs no hypervisor, so a separate build of the library can run it against this crate.
    #[test]
    #[ignore = "requires an agent library in EDGE_AGENT_TEST_LIBRARY"]
    fn a_separately_built_library_binds_the_sandbox_abi() {
        let path = std::path::absolute(
            std::env::var_os("EDGE_AGENT_TEST_LIBRARY")
                .expect("EDGE_AGENT_TEST_LIBRARY must be set"),
        )
        .unwrap();
        let digest: [u8; 32] = Sha256::digest(fs::read(&path).unwrap()).into();
        let library = load(&path, &digest).unwrap();
        assert!(ptr::eq(library, load(&path, &digest).unwrap()));
        let mismatch = load(&path, &[0; 32]).unwrap_err();
        assert_eq!(mismatch.code(), ErrorCode::BackendUnavailable, "{mismatch}");

        let malformed = library
            .host_open(&serde_json::json!({ "schemaVersion": 1 }))
            .unwrap_err();
        assert_eq!(malformed.code(), ErrorCode::MalformedRequest, "{malformed}");

        let refused = |result: Result<()>| {
            let error = result.unwrap_err();
            assert!(error.message().contains("unknown handle"), "{error}");
        };
        let (handle, value) = (u64::MAX, serde_json::json!({}));
        type Value = serde_json::Value;
        refused(library.host_info::<Value>(handle).map(drop));
        refused(library.capabilities::<Value>(handle).map(drop));
        refused(library.probe(handle));
        refused(library.validate_provision(handle, &value));
        refused(library.validate_exec(handle, &value));
        refused(library.provision::<Value>(handle, &value).map(drop));
        refused(library.start::<Value>(handle, "token").map(drop));
        refused(library.stop::<Value>(handle, "token").map(drop));
        refused(library.deprovision::<Value>(handle, "token").map(drop));
        refused(library.diagnostics::<Value>(handle, "token").map(drop));
        refused(library.guest_logs(handle, "token").map(drop));
        refused(library.exec(handle, "token", &value).map(drop));
        refused(library.exec_next(handle, Some(0), |_| ()).map(drop));
        refused(library.exec_write(handle, b"input", Some(0)).map(drop));
        refused(library.exec_close_input(handle));
        refused(library.exec_cancel(handle));
        refused(library.image_register::<Value>(handle, &value).map(drop));
        refused(library.image_list::<Value>(handle).map(drop));
        refused(library.image_verify(handle, "image"));
        refused(library.image_unregister(handle, "image").map(drop));
        library.exec_release(handle);
        library.host_close(handle);
    }

    #[test]
    fn an_event_whose_data_is_missing_is_a_backend_error() {
        let exec = fake::execution(fake::Next::OutputWithoutData, fake::Write::Accept);
        let error = fake::library()
            .exec_next(exec, Some(0), |_| ())
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::BackendError, "{error}");
        assert_eq!(
            error.message(),
            "the agent library returned an execution event whose data is missing"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_library_open_for_loading_keeps_writers_and_renames_out() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("agent");
        fs::create_dir(&directory).unwrap();
        let path = directory.join("aci_edge_agent.dll");
        fs::write(&path, b"library").unwrap();
        let file = open_for_load(&path).unwrap();
        assert!(OpenOptions::new().write(true).open(&path).is_err());
        assert!(fs::remove_file(&path).is_err());
        assert!(fs::rename(&path, directory.join("other.dll")).is_err());
        assert!(fs::rename(&directory, root.path().join("other")).is_err());
        drop(file);
        fs::rename(&directory, root.path().join("other")).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_sealed_copy_holds_the_checked_bytes_and_refuses_changes() {
        use std::io::Write;
        let mut copy = sealed_copy(b"library").unwrap();
        assert_eq!(fs::read(descriptor_path(&copy)).unwrap(), b"library");
        assert!(copy.write_all(b"other").is_err());
        assert!(copy.set_len(0).is_err());
        assert_eq!(fs::read(descriptor_path(&copy)).unwrap(), b"library");
    }
}

/// A test double of the library's execution exports, which drives an execution through failures
/// that a guest cannot produce on demand. Its other exports refuse every handle.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Condvar, MutexGuard};

    use aci_edge_sandboxes_model::wire::ExecEnd;

    use super::*;
    use crate::exec::ExecOutcome;

    /// How an execution answers `exec_next`.
    #[derive(Clone, Copy)]
    pub(crate) enum Next {
        /// Fails at once, as a library error would.
        Fail,
        /// Ends with [`ExecOutcome::Cancelled`] once the execution is cancelled.
        ExitWhenCancelled,
        /// Delivers this many bytes of standard output, then ends with [`ExecOutcome::Exited`] 0.
        OutputThenExit(usize),
        /// Delivers standard output with a length but a null data pointer, as a faulty library
        /// might.
        OutputWithoutData,
    }

    /// How an execution answers `exec_write`.
    #[derive(Clone, Copy)]
    pub(crate) enum Write {
        /// Accepts every byte.
        Accept,
        /// Waits for input credit that never comes, and fails once the execution is cancelled.
        BlockUntilCancelled,
    }

    struct State {
        next: Next,
        write: Write,
        cancelled: bool,
        calls: Vec<&'static str>,
        end: Vec<u8>,
        sent: usize,
    }

    static EXECUTIONS: Mutex<BTreeMap<u64, State>> = Mutex::new(BTreeMap::new());
    static CHANGED: Condvar = Condvar::new();
    static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

    fn executions() -> MutexGuard<'static, BTreeMap<u64, State>> {
        EXECUTIONS.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Returns the double, whose executions are those that [`execution`] registers.
    pub(crate) fn library() -> &'static Library {
        static LIBRARY: OnceLock<Library> = OnceLock::new();
        LIBRARY.get_or_init(|| Library {
            exports: Exports {
                host_open: refuse_open,
                host_close: refuse_release,
                host_info: refuse_query,
                capabilities: refuse_query,
                probe: refuse_query,
                validate_provision: refuse_call,
                validate_exec: refuse_call,
                provision: refuse_call,
                start: refuse_call,
                stop: refuse_call,
                deprovision: refuse_call,
                diagnostics: refuse_call,
                guest_logs: refuse_call,
                exec: refuse_exec,
                exec_next: next,
                exec_write: write,
                exec_close_input: close_input,
                exec_cancel: cancel,
                exec_release: release,
                image_register: refuse_call,
                image_list: refuse_query,
                image_verify: refuse_call,
                image_unregister: refuse_call,
                buffer_free: free,
            },
            _library: None,
            _image: None,
        })
    }

    /// Registers an execution that behaves as `next` and `write` say, and returns its handle.
    pub(crate) fn execution(next: Next, write: Write) -> u64 {
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        let outcome = match next {
            Next::OutputThenExit(_) => ExecOutcome::Exited(0),
            Next::Fail | Next::ExitWhenCancelled | Next::OutputWithoutData => {
                ExecOutcome::Cancelled
            }
        };
        let end = serde_json::to_vec(&ExecEnd::Outcome(outcome)).unwrap();
        executions().insert(
            handle,
            State {
                next,
                write,
                cancelled: false,
                calls: Vec::new(),
                end,
                sent: 0,
            },
        );
        handle
    }

    /// The cancel, close-input, and release calls that an execution received, in order.
    pub(crate) fn calls(handle: u64) -> Vec<&'static str> {
        executions()
            .get(&handle)
            .map(|state| state.calls.clone())
            .unwrap_or_default()
    }

    fn record(handle: u64, call: &'static str) -> i32 {
        let mut executions = executions();
        let Some(state) = executions.get_mut(&handle) else {
            return STATUS_INVALID_HANDLE;
        };
        state.calls.push(call);
        if call == "cancel" {
            state.cancelled = true;
            CHANGED.notify_all();
        }
        STATUS_OK
    }

    unsafe extern "C" fn next(handle: u64, _timeout: i64, event: *mut ExecEvent) -> i32 {
        static OUTPUT: [u8; 64 * 1024] = [b'x'; 64 * 1024];
        let mut executions = executions();
        loop {
            let Some(state) = executions.get_mut(&handle) else {
                return STATUS_INVALID_HANDLE;
            };
            let ready = match state.next {
                Next::Fail => return STATUS_INVALID_ARGUMENT,
                Next::ExitWhenCancelled if !state.cancelled => None,
                Next::OutputThenExit(bytes) if state.sent < bytes => {
                    let length = (bytes - state.sent).min(OUTPUT.len());
                    state.sent += length;
                    Some((EVENT_STDOUT, OUTPUT.as_ptr(), length))
                }
                Next::ExitWhenCancelled | Next::OutputThenExit(_) => {
                    Some((EVENT_EXIT, state.end.as_ptr(), state.end.len()))
                }
                Next::OutputWithoutData => Some((EVENT_STDOUT, ptr::null(), 1)),
            };
            if let Some((kind, data, length)) = ready {
                // SAFETY: the caller passes a writable event, and `OUTPUT` is static while `end`
                // lives as long as the registration.
                unsafe { *event = ExecEvent { kind, data, length } };
                return STATUS_OK;
            }
            executions = CHANGED
                .wait(executions)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    unsafe extern "C" fn write(
        handle: u64,
        _data: *const u8,
        length: usize,
        _timeout: i64,
        written: *mut usize,
        _out: *mut *mut u8,
        _out_length: *mut usize,
    ) -> i32 {
        let mut executions = executions();
        loop {
            let Some(state) = executions.get(&handle) else {
                return STATUS_INVALID_HANDLE;
            };
            match state.write {
                Write::Accept => {
                    // SAFETY: the caller passes a writable count.
                    unsafe { *written = length };
                    return STATUS_OK;
                }
                Write::BlockUntilCancelled if state.cancelled => return STATUS_INVALID_ARGUMENT,
                Write::BlockUntilCancelled => {}
            }
            executions = CHANGED
                .wait(executions)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    unsafe extern "C" fn close_input(handle: u64, _: *mut *mut u8, _: *mut usize) -> i32 {
        record(handle, "close_input")
    }

    unsafe extern "C" fn cancel(handle: u64, _: *mut *mut u8, _: *mut usize) -> i32 {
        record(handle, "cancel")
    }

    unsafe extern "C" fn release(handle: u64) -> i32 {
        record(handle, "release")
    }

    unsafe extern "C" fn refuse_open(
        _: *const u8,
        _: usize,
        _: *mut u64,
        _: *mut *mut u8,
        _: *mut usize,
    ) -> i32 {
        STATUS_INVALID_HANDLE
    }

    unsafe extern "C" fn refuse_release(_: u64) -> i32 {
        STATUS_INVALID_HANDLE
    }

    unsafe extern "C" fn refuse_query(_: u64, _: *mut *mut u8, _: *mut usize) -> i32 {
        STATUS_INVALID_HANDLE
    }

    unsafe extern "C" fn refuse_call(
        _: u64,
        _: *const u8,
        _: usize,
        _: *mut *mut u8,
        _: *mut usize,
    ) -> i32 {
        STATUS_INVALID_HANDLE
    }

    #[allow(clippy::too_many_arguments)]
    unsafe extern "C" fn refuse_exec(
        _: u64,
        _: *const u8,
        _: usize,
        _: *const u8,
        _: usize,
        _: *mut u64,
        _: *mut *mut u8,
        _: *mut usize,
    ) -> i32 {
        STATUS_INVALID_HANDLE
    }

    unsafe extern "C" fn free(_: *mut u8, _: usize) {}
}
