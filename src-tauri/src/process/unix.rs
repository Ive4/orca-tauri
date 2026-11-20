//! Unix compatibility, with explicit limits:
//! - normal exit: SIGTERM then SIGKILL of our private process group;
//! - Linux parent death: PR_SET_PDEATHSIG also protects the direct child;
//! - macOS hard kills and descendants that call setsid()/setpgid() require a
//!   separate supervisor/cgroup policy and are NOT covered by this module.
//!
//! Never describe this fallback as Windows Job Object-equivalent containment.

use super::{LaunchSpec, HOST_ENV_BLOCKLIST};
use anyhow::{Context, Result};
use std::{
    io,
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
};

pub(super) struct PlatformProcess {
    child: Child,
    group: libc::pid_t,
    kill_sent: bool,
    stdout: Option<os_pipe::PipeReader>,
    stderr: Option<os_pipe::PipeReader>,
}

impl PlatformProcess {
    pub(super) fn spawn(spec: &LaunchSpec) -> Result<Self> {
        let (reader, writer) = os_pipe::pipe()?;
        let (stderr_reader, stderr_writer) = os_pipe::pipe()?;
        let mut command = Command::new(&spec.program);
        configure_environment(&mut command, &spec.env);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(writer))
            .stderr(Stdio::from(stderr_writer));
        #[cfg(target_os = "linux")]
        let parent = unsafe { libc::getpid() };
        // Only async-signal-safe syscalls inside pre_exec, never logging/locks.
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::getppid() != parent {
                        libc::_exit(127);
                    }
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .context("Could not spawn core in a private process group")?;
        let group = child.id() as libc::pid_t;
        Ok(Self {
            child,
            group,
            kill_sent: false,
            stdout: Some(reader),
            stderr: Some(stderr_reader),
        })
    }

    pub(super) fn id(&self) -> u32 {
        self.child.id()
    }
    pub(super) fn take_stdout(&mut self) -> Option<os_pipe::PipeReader> {
        self.stdout.take()
    }
    pub(super) fn take_stderr(&mut self) -> Option<os_pipe::PipeReader> {
        self.stderr.take()
    }
    pub(super) fn try_wait(&mut self) -> Result<Option<String>> {
        Ok(self.child.try_wait()?.map(|status| status.to_string()))
    }

    fn signal_group(&self, signal: libc::c_int) -> Result<()> {
        if unsafe { libc::killpg(self.group, signal) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error.into());
            }
        }
        Ok(())
    }

    pub(super) fn request_graceful_stop(&mut self) -> Result<bool> {
        if self.kill_sent {
            return Ok(false);
        }
        self.signal_group(libc::SIGTERM)?;
        Ok(true)
    }

    pub(super) fn kill_tree(&mut self) -> Result<()> {
        // macOS can return EPERM when the group has exited but is not yet reaped.
        if !self.kill_sent {
            self.signal_group(libc::SIGKILL)?;
            self.kill_sent = true;
        }
        Ok(())
    }

    pub(super) fn tree_empty(&mut self) -> Result<bool> {
        // wait()/try_wait() reaps our direct child. Unix does not expose a Job
        // Object-like complete-tree accounting primitive; detached children are
        // deliberately outside this fallback's claim.
        Ok(self.child.try_wait()?.is_some())
    }
}

fn configure_environment(
    command: &mut Command,
    overrides: &std::collections::BTreeMap<String, String>,
) {
    for key in HOST_ENV_BLOCKLIST {
        command.env_remove(key);
    }
    command.envs(overrides);
}

impl Drop for PlatformProcess {
    fn drop(&mut self) {
        let _ = self.kill_tree();
        let _ = self.child.try_wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, ffi::OsStr};

    #[test]
    fn excludes_host_hooks_then_allows_explicit_node_options() {
        let mut command = Command::new("unused-test-command");
        command.env("NODE_OPTIONS", "unwanted host preload");
        command.env("ORCA_APP_EXECUTABLE", "/host/electron");
        configure_environment(
            &mut command,
            &BTreeMap::from([("NODE_OPTIONS".into(), "--max-old-space-size=512".into())]),
        );
        let environment: BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(
            environment.get(OsStr::new("ELECTRON_RUN_AS_NODE")),
            Some(&None)
        );
        assert_eq!(
            environment.get(OsStr::new("ORCA_APP_EXECUTABLE")),
            Some(&None)
        );
        assert_eq!(
            environment.get(OsStr::new("NODE_OPTIONS")),
            Some(&Some(OsStr::new("--max-old-space-size=512")))
        );
    }
}
