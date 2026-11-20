//! Windows 10+ creation-time containment (Windows 11 is the primary target).
//!
//! Do NOT replace PROC_THREAD_ATTRIBUTE_JOB_LIST with spawn-then-assign: even a
//! suspended child can be orphaned if its owner crashes before assignment.
//! https://devblogs.microsoft.com/oldnewthing/20230209-00/?p=107812/

use super::{LaunchSpec, HOST_ENV_BLOCKLIST};
use anyhow::{bail, ensure, Context, Result};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::File,
    mem::{size_of, size_of_val},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
};
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::{
            DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, WAIT_FAILED, WAIT_OBJECT_0,
            WAIT_TIMEOUT,
        },
        System::{
            JobObjects::{
                CreateJobObjectW, JobObjectBasicAccountingInformation,
                JobObjectExtendedLimitInformation, QueryInformationJobObject,
                SetInformationJobObject, TerminateJobObject,
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Threading::{
                CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
                GetExitCodeProcess, InitializeProcThreadAttributeList, ResumeThread,
                UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
                CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
                LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST,
                STARTF_USESTDHANDLES, STARTUPINFOEXW,
            },
        },
    },
};

pub(super) struct PlatformProcess {
    // OwnedHandle is Send; ownership is never duplicated into the child. Keeping
    // this first also closes the job before the process handle on field drop.
    job: OwnedHandle,
    process: OwnedHandle,
    pid: u32,
    stdout: Option<os_pipe::PipeReader>,
    stderr: Option<os_pipe::PipeReader>,
}

impl PlatformProcess {
    pub(super) fn spawn(spec: &LaunchSpec) -> Result<Self> {
        ensure!(
            spec.program
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exe")),
            "Windows core entry must be a native .exe, never a batch file"
        );
        let application = nul_terminated(spec.program.as_os_str())?;
        let directory = nul_terminated(spec.cwd.as_os_str())?;
        let mut command_line = command_line(spec.program.as_os_str(), &spec.args)?;
        let environment = environment_block(&spec.env)?;
        let (reader, writer) = os_pipe::pipe()?;
        let (stderr_reader, stderr_writer) = os_pipe::pipe()?;
        let stdin_file = File::open("NUL").context("Could not open NUL for core stdin")?;
        let stdout = inheritable_duplicate(&writer)?;
        let stderr = inheritable_duplicate(&stderr_writer)?;
        let stdin = inheritable_duplicate(&stdin_file)?;

        // Null SECURITY_ATTRIBUTES makes this unnamed Job handle non-inheritable.
        // No BREAKAWAY_OK / SILENT_BREAKAWAY_OK is ever enabled.
        let job = owned(unsafe { CreateJobObjectW(None, PCWSTR::null()) }?)?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                raw(&job),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .context("Could not configure kill-on-close Job Object")?;

        // Values referenced by an attribute list must outlive that list.
        let jobs = [raw(&job)];
        let handles = [raw(&stdin), raw(&stdout), raw(&stderr)];
        let attributes = AttributeList::new(2)?;
        unsafe {
            UpdateProcThreadAttribute(
                attributes.raw,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                Some(jobs.as_ptr().cast()),
                size_of_val(&jobs),
                None,
                None,
            )?;
            UpdateProcThreadAttribute(
                attributes.raw,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(handles.as_ptr().cast()),
                size_of_val(&handles),
                None,
                None,
            )?;
        }
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = raw(&stdin);
        startup.StartupInfo.hStdOutput = raw(&stdout);
        startup.StartupInfo.hStdError = raw(&stderr);
        startup.lpAttributeList = attributes.raw;
        let mut info = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessW(
                PCWSTR(application.as_ptr()),
                Some(PWSTR(command_line.as_mut_ptr())),
                None,
                None,
                true,
                CREATE_SUSPENDED
                    | CREATE_NO_WINDOW
                    | CREATE_UNICODE_ENVIRONMENT
                    | EXTENDED_STARTUPINFO_PRESENT,
                Some(environment.as_ptr().cast()),
                PCWSTR(directory.as_ptr()),
                &startup.StartupInfo,
                &mut info,
            )
        }
        .context("Could not create core inside its Job Object")?;

        // CreateProcessW only yields valid handles after success; both handles
        // immediately acquire RAII owners, including ResumeThread error paths.
        let process = owned(info.hProcess)?;
        let thread = owned(info.hThread)?;
        if unsafe { ResumeThread(raw(&thread)) } == u32::MAX {
            let error = windows::core::Error::from_win32();
            let _ = unsafe { TerminateJobObject(raw(&job), 1) };
            let _ = unsafe { WaitForSingleObject(raw(&process), 5000) };
            return Err(error).context("Could not resume the contained core");
        }
        // The initial thread handle and our copies of pipe writers close here.
        // Only the explicitly listed standard handles cross into the child.
        Ok(Self {
            job,
            process,
            pid: info.dwProcessId,
            stdout: Some(reader),
            stderr: Some(stderr_reader),
        })
    }

    pub(super) fn id(&self) -> u32 {
        self.pid
    }
    pub(super) fn take_stdout(&mut self) -> Option<os_pipe::PipeReader> {
        self.stdout.take()
    }
    pub(super) fn take_stderr(&mut self) -> Option<os_pipe::PipeReader> {
        self.stderr.take()
    }

    pub(super) fn try_wait(&mut self) -> Result<Option<String>> {
        match unsafe { WaitForSingleObject(raw(&self.process), 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                unsafe { GetExitCodeProcess(raw(&self.process), &mut code) }?;
                Ok(Some(format!("exit code {code}")))
            }
            WAIT_FAILED => Err(windows::core::Error::from_win32().into()),
            _ => bail!("Unexpected process wait result"),
        }
    }

    pub(super) fn request_graceful_stop(&mut self) -> Result<bool> {
        // A CREATE_NO_WINDOW process has no shared console for CTRL_BREAK.
        // Graceful stop requires an explicitly configured upstream protocol.
        Ok(false)
    }

    pub(super) fn kill_tree(&mut self) -> Result<()> {
        unsafe { TerminateJobObject(raw(&self.job), 1) }
            .context("Could not terminate the core Job Object")
    }

    pub(super) fn tree_empty(&mut self) -> Result<bool> {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        unsafe {
            QueryInformationJobObject(
                Some(raw(&self.job)),
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                None,
            )
        }?;
        Ok(accounting.ActiveProcesses == 0)
    }
}

impl Drop for PlatformProcess {
    fn drop(&mut self) {
        let _ = self.kill_tree();
        // The final non-inherited Job handle closes even if termination errors.
    }
}

fn raw(handle: &OwnedHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

fn owned(handle: HANDLE) -> Result<OwnedHandle> {
    ensure!(!handle.is_invalid(), "Win32 returned an invalid handle");
    // SAFETY: caller passes exactly one newly owned kernel handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

fn inheritable_duplicate(handle: &impl AsRawHandle) -> Result<OwnedHandle> {
    let mut duplicate = HANDLE::default();
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            HANDLE(handle.as_raw_handle()),
            GetCurrentProcess(),
            &mut duplicate,
            0,
            true,
            DUPLICATE_SAME_ACCESS,
        )
    }?;
    owned(duplicate)
}

struct AttributeList {
    // Retain aligned allocation for the lifetime of the initialized list.
    _storage: Vec<usize>,
    raw: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl AttributeList {
    fn new(count: u32) -> Result<Self> {
        let mut size = 0usize;
        let _ = unsafe { InitializeProcThreadAttributeList(None, count, Some(0), &mut size) };
        ensure!(size > 0, "Could not determine process attribute list size");
        let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
        let pointer = LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast());
        unsafe { InitializeProcThreadAttributeList(Some(pointer), count, Some(0), &mut size) }?;
        Ok(Self {
            _storage: storage,
            raw: pointer,
        })
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.raw) };
    }
}

fn nul_terminated(value: &OsStr) -> Result<Vec<u16>> {
    let mut wide: Vec<u16> = value.encode_wide().collect();
    ensure!(!wide.contains(&0), "Windows launch value contains NUL");
    wide.push(0);
    Ok(wide)
}

// Follow the CRT argument quoting rules; CreateProcess is invoked directly,
// never through cmd.exe. lpApplicationName separately fixes executable lookup.
fn quoted_arg(value: &OsStr) -> Result<Vec<u16>> {
    let wide: Vec<u16> = value.encode_wide().collect();
    ensure!(!wide.contains(&0), "Windows argument contains NUL");
    let mut output = vec![b'"' as u16];
    let mut slashes = 0;
    for unit in wide {
        if unit == b'\\' as u16 {
            slashes += 1;
        } else {
            if unit == b'"' as u16 {
                output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2 + 1));
            } else {
                output.extend(std::iter::repeat_n(b'\\' as u16, slashes));
            }
            output.push(unit);
            slashes = 0;
        }
    }
    output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    output.push(b'"' as u16);
    Ok(output)
}

fn command_line(program: &OsStr, args: &[String]) -> Result<Vec<u16>> {
    let mut line = quoted_arg(program)?;
    for argument in args {
        line.push(b' ' as u16);
        line.extend(quoted_arg(OsStr::new(argument))?);
    }
    ensure!(
        line.len() < 32767,
        "Core command line exceeds the Windows limit"
    );
    line.push(0);
    Ok(line)
}

fn environment_block(overrides: &BTreeMap<String, String>) -> Result<Vec<u16>> {
    environment_block_from(std::env::vars_os(), overrides)
}

fn environment_block_from(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    overrides: &BTreeMap<String, String>,
) -> Result<Vec<u16>> {
    // Preserve the inherited environment, replacing keys case-insensitively so
    // Path/PATH cannot create duplicate entries. Values stay lossless UTF-16.
    let mut values: BTreeMap<String, (OsString, OsString)> = inherited
        .into_iter()
        .filter(|(key, _)| {
            !HOST_ENV_BLOCKLIST
                .iter()
                .any(|blocked| key.eq_ignore_ascii_case(blocked))
        })
        .map(|(key, value)| (key.to_string_lossy().to_uppercase(), (key, value)))
        .collect();
    for (key, value) in overrides {
        ensure!(
            !key.is_empty() && !key.contains(['=', '\0']) && !value.contains('\0'),
            "Invalid environment override"
        );
        values.insert(
            key.to_uppercase(),
            (OsString::from(key), OsString::from(value)),
        );
    }
    let mut output = Vec::new();
    for (_, (key, value)) in values {
        output.extend(key.encode_wide());
        output.push(b'=' as u16);
        output.extend(value.encode_wide());
        output.push(0);
    }
    output.push(0);
    if output.len() == 1 {
        output.push(0);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_empty_spaces_quotes_and_trailing_backslash() {
        let quote =
            |input: &str| String::from_utf16(&quoted_arg(OsStr::new(input)).unwrap()).unwrap();
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote("two words"), "\"two words\"");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote("C:\\with space\\"), "\"C:\\with space\\\\\"");
    }

    #[test]
    fn environment_replaces_path_case_insensitively() {
        let overrides = BTreeMap::from([("pAtH".to_owned(), "SENTINEL_PATH".to_owned())]);
        let block = String::from_utf16(&environment_block(&overrides).unwrap()).unwrap();
        let paths: Vec<_> = block
            .split('\0')
            .filter(|part| part.to_ascii_uppercase().starts_with("PATH="))
            .collect();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], "pAtH=SENTINEL_PATH");
        assert!(block.ends_with("\0\0"));
    }

    #[test]
    fn excludes_host_hooks_case_insensitively_before_overrides() {
        let inherited = [
            (OsString::from("eLeCtRoN_rUn_As_NoDe"), OsString::from("1")),
            (
                OsString::from("orca_app_executable"),
                OsString::from("C:\\host\\electron.exe"),
            ),
            (
                OsString::from("Node_Options"),
                OsString::from("unwanted host preload"),
            ),
            (OsString::from("PATH"), OsString::from("safe path")),
        ];
        let overrides =
            BTreeMap::from([("NODE_OPTIONS".into(), "--max-old-space-size=512".into())]);
        let block =
            String::from_utf16(&environment_block_from(inherited, &overrides).unwrap()).unwrap();
        assert!(!block.to_ascii_uppercase().contains("ELECTRON_RUN_AS_NODE"));
        assert!(!block.to_ascii_uppercase().contains("ORCA_APP_EXECUTABLE"));
        assert!(!block.contains("unwanted host preload"));
        assert!(block.contains("NODE_OPTIONS=--max-old-space-size=512\0"));
        assert!(block.contains("PATH=safe path\0"));
    }
}
