use tty::ffi::openpty;
use libc;
use std::io::{self, Read, Write};

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::fs::File;
use std::process::{Child, Command, Stdio};

use mio::unix::{EventedFd, UnixReady};
use mio::{self, PollOpt, Ready, Token};
use mio::event::Evented;

use tokio_core::reactor::{Handle, PollEvented};

pub struct Pty {
    master: PollEvented<Fd<File>>,
    slave: Option<File>,
    path: PathBuf,
}

impl Pty {
    pub fn new<T>(template: Option<&T>, handle: &Handle) -> io::Result<Self>
    where
        T: AsRawFd,
    {
        let pty = match template {
            Some(_t) => unimplemented!(),
            // FIXME
            // openpty(
            //     Some(&Termios::from_fd(t.as_raw_fd())?),
            //     Some(&get_winsize(t)?),
            // )?,
            None => openpty(None, None)?,
        };

        Ok(Pty {
            master: make_evented(pty.master, handle).unwrap(),
            slave: Some(pty.slave),
            path: pty.path,
        })
    }

    /// Spawn a new process connected to the slave TTY
    pub fn spawn(&mut self, mut cmd: Command) -> io::Result<Child> {
        match self.slave.take() {
            Some(slave) => {
                // Force new session
                // TODO: tcsetpgrp
                cmd.stdin(unsafe { Stdio::from_raw_fd(slave.as_raw_fd()) }).
                    stdout(unsafe { Stdio::from_raw_fd(slave.as_raw_fd()) }).
                    // Must close the slave FD to not wait indefinitely the end of the proxy
                    stderr(unsafe { Stdio::from_raw_fd(slave.into_raw_fd()) }).
                    // Don't check the error of setsid because it fails if we're the
                    // process leader already. We just forked so it shouldn't return
                    // error, but ignore it anyway.
                    before_exec(|| { let _ = unsafe { libc::setsid() }; Ok(()) }).
                    spawn()
            }
            None => Err(io::Error::new(io::ErrorKind::BrokenPipe, "No TTY slave")),
        }
    }
}

impl Read for Pty {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.master.read(buf)
    }
}

impl Write for Pty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.master.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.master.flush()
    }
}

impl AsRef<Path> for Pty {
    fn as_ref(&self) -> &Path {
        self.path.as_ref()
    }
}

#[derive(Debug)]
pub struct Fd<T>(T);

impl<T> Evented for Fd<T>
where
    T: AsRawFd,
{
    fn register(
        &self,
        poll: &mio::Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        EventedFd(&self.0.as_raw_fd()).register(poll, token, interest | UnixReady::hup(), opts)
    }

    fn reregister(
        &self,
        poll: &mio::Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        EventedFd(&self.0.as_raw_fd()).reregister(poll, token, interest | UnixReady::hup(), opts)
    }

    fn deregister(&self, poll: &mio::Poll) -> io::Result<()> {
        EventedFd(&self.0.as_raw_fd()).deregister(poll)
    }
}

impl<T: io::Read> io::Read for Fd<T> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(bytes)
    }
}

impl<T: io::Write> io::Write for Fd<T> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

fn make_evented<T>(io: T, handle: &Handle) -> io::Result<PollEvented<Fd<T>>>
where
    T: AsRawFd,
{
    // Set the fd to nonblocking before we pass it to the event loop
    unsafe {
        let fd = io.as_raw_fd();

        let r = libc::fcntl(fd, libc::F_GETFL);
        if r == -1 {
            return Err(io::Error::last_os_error());
        }

        let r = libc::fcntl(fd, libc::F_SETFL, r | libc::O_NONBLOCK);
        if r == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    let io = PollEvented::new(Fd(io), handle)?;
    Ok(io)
}
