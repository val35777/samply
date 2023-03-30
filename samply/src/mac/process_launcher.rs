use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::mem;
use std::os::unix::prelude::OsStrExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

pub use super::mach_ipc::{mach_port_t, MachError, OsIpcSender};
use super::mach_ipc::{BlockingMode, OsIpcMultiShotServer, MACH_PORT_NULL};
use flate2::write::GzDecoder;
use tempfile::tempdir;

pub struct TaskAccepter {
    server: OsIpcMultiShotServer,
    _temp_dir: tempfile::TempDir,
}

static PRELOAD_LIB_CONTENTS: &[u8] =
    include_bytes!("../../resources/libsamply_mac_preload.dylib.gz");

impl TaskAccepter {
    pub fn create_and_launch_root_task<I, S>(
        program: S,
        args: I,
    ) -> Result<(Self, Child), MachError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (server, server_name) = OsIpcMultiShotServer::new()?;

        // Launch the child with DYLD_INSERT_LIBRARIES set to libsamply_mac_preload.dylib.

        // We would like to ship with libsamply_mac_preload.dylib as a separate resource file.
        // But this won't work with cargo install. So we write out libsamply_mac_preload.dylib
        // to a temporary directory.
        let dir = tempdir().expect("Couldn't create temporary directory for preload-lib");
        let preload_lib_path = dir.path().join("libsamply_mac_preload.dylib");
        let file =
            File::create(&preload_lib_path).expect("Couldn't create libsamply_mac_preload.dylib");
        let mut decoder = GzDecoder::new(file);
        decoder
            .write_all(PRELOAD_LIB_CONTENTS)
            .expect("Couldn't write libsamply_mac_preload.dylib (error during write_all)");
        decoder
            .finish()
            .expect("Couldn't write libsamply_mac_preload.dylib (error during finish)");

        // Take this process's environment variables and add DYLD_INSERT_LIBRARIES
        // and SAMPLY_BOOTSTRAP_SERVER_NAME.
        let child_env = std::env::vars_os()
            .chain(std::iter::once((
                "DYLD_INSERT_LIBRARIES".into(),
                preload_lib_path.into(),
            )))
            .chain(std::iter::once((
                "SAMPLY_BOOTSTRAP_SERVER_NAME".into(),
                server_name.into(),
            )));

        let root_child = match Command::new(program.as_ref())
            .args(args)
            .envs(child_env)
            .spawn()
        {
            Ok(child) => child,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "Error: Could not find an executable with the name {}.",
                    program.as_ref().to_string_lossy()
                );
                std::process::exit(1)
            }
            Err(err) => {
                eprintln!("Error: Could not launch child process: {err}");
                std::process::exit(1)
            }
        };

        Ok((
            TaskAccepter {
                server,
                _temp_dir: dir,
            },
            root_child,
        ))
    }

    pub fn next_message(&mut self, timeout: Duration) -> Result<ReceivedStuff, MachError> {
        // Wait until the child is ready
        let (res, mut channels, _) = self
            .server
            .accept(BlockingMode::BlockingWithTimeout(timeout))?;
        let received_stuff = match res.split_at(7) {
            (b"My task", pid_bytes) => {
                assert!(pid_bytes.len() == 4);
                let pid =
                    u32::from_le_bytes([pid_bytes[0], pid_bytes[1], pid_bytes[2], pid_bytes[3]]);
                let task_channel = channels.pop().unwrap();
                let sender_channel = channels.pop().unwrap();
                let sender_channel = sender_channel.into_sender();

                let task = task_channel.into_port();

                ReceivedStuff::AcceptedTask(AcceptedTask {
                    task,
                    pid,
                    sender_channel,
                })
            }
            (b"Jitdump", jitdump_info) => {
                let pid_bytes = &jitdump_info[0..4];
                let pid =
                    u32::from_le_bytes([pid_bytes[0], pid_bytes[1], pid_bytes[2], pid_bytes[3]]);
                let len = jitdump_info[4] as usize;
                let path = &jitdump_info[5..][..len];
                ReceivedStuff::JitdumpPath(pid, OsStr::from_bytes(path).into())
            }
            (other, _) => {
                panic!("Unexpected message: {:?}", other);
            }
        };
        Ok(received_stuff)
    }
}

pub enum ReceivedStuff {
    AcceptedTask(AcceptedTask),
    JitdumpPath(u32, PathBuf),
}

pub struct AcceptedTask {
    task: mach_port_t,
    pid: u32,
    sender_channel: OsIpcSender,
}

impl AcceptedTask {
    pub fn take_task(&mut self) -> mach_port_t {
        mem::replace(&mut self.task, MACH_PORT_NULL)
    }

    pub fn get_id(&self) -> u32 {
        self.pid
    }

    pub fn start_execution(&self) {
        self.sender_channel.send(b"Proceed", vec![]).unwrap();
    }
}
