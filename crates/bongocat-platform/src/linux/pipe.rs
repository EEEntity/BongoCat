//! Parent-owned pipe and subprocess; dropping it closes the helper's lifetime pipe.
use super::protocol::{HEADER, InputMessage, PACKET_SIZE};
use crate::PlatformInputError;
use std::{
    io::{self, Read},
    os::fd::AsRawFd,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

pub(super) struct InputHelper {
    child: Child,
    control: Option<ChildStdin>,
    output: ChildStdout,
    header_read: bool,
    bytes: [u8; PACKET_SIZE],
    offset: usize,
}

impl InputHelper {
    pub(super) fn start() -> Result<Self, PlatformInputError> {
        let executable =
            std::env::current_exe().map_err(|_| PlatformInputError::BackendUnavailable)?;
        let mut child = Command::new("/usr/bin/pkexec")
            .arg("--disable-internal-agent")
            .arg(executable)
            .arg("--input-helper")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| PlatformInputError::BackendUnavailable)?;
        let control = child.stdin.take();
        let output = child.stdout.take().expect("spawn configured piped stdout");
        let owner = Self {
            child,
            control,
            output,
            header_read: false,
            bytes: [0; PACKET_SIZE],
            offset: 0,
        };
        // SAFETY: output owns this valid fd for the complete fcntl calls.
        let flags = unsafe { libc::fcntl(owner.output.as_raw_fd(), libc::F_GETFL) };
        if flags == -1
            || unsafe {
                libc::fcntl(
                    owner.output.as_raw_fd(),
                    libc::F_SETFL,
                    flags | libc::O_NONBLOCK,
                )
            } == -1
        {
            return Err(PlatformInputError::BackendUnavailable);
        }
        Ok(owner)
    }

    pub(super) fn read_message(&mut self) -> Result<Option<InputMessage>, PlatformInputError> {
        loop {
            let size = if self.header_read {
                PACKET_SIZE
            } else {
                HEADER.len()
            };
            match self.output.read(&mut self.bytes[self.offset..size]) {
                Ok(0) => {
                    return Err(if self.header_read {
                        PlatformInputError::BackendUnavailable
                    } else {
                        PlatformInputError::PermissionDenied
                    });
                }
                Ok(count) => self.offset += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(PlatformInputError::BackendUnavailable),
            }
            if self.offset != size {
                continue;
            }
            self.offset = 0;
            if !self.header_read {
                if &self.bytes[..HEADER.len()] != HEADER {
                    return Err(PlatformInputError::BackendUnavailable);
                }
                self.header_read = true;
                continue;
            }
            return InputMessage::decode(&self.bytes)
                .map(Some)
                .ok_or(PlatformInputError::BackendUnavailable);
        }
    }
}

impl Drop for InputHelper {
    fn drop(&mut self) {
        // EOF ends the elevated child. kill also cancels a pending, unprivileged
        // pkexec authentication; after elevation it may be denied, so EOF owns shutdown.
        drop(self.control.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
