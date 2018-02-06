use std::error::Error;
use std::fmt;
use std::io;

/// An error returned when initi [`Expect.init()`](../expect/struct.Expect.html#method.init) fails
#[derive(Debug)]
pub enum InitError {
    /// Failed to start the event loop that manages the expect sessions
    Reactor(io::Error),
    /// Failed to start the thread in which the event loop runs
    SpawnWorker(io::Error),
}

impl Error for InitError {
    fn description(&self) -> &str {
        match *self {
            InitError::Reactor(_) => "failed to start the event loop",
            InitError::SpawnWorker(_) => "failed to spawn an expect thread",
        }
    }

    fn cause(&self) -> Option<&Error> {
        match *self {
            InitError::Reactor(ref e) | InitError::SpawnWorker(ref e) => Some(e),
        }
    }
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        if let Some(cause) = self.cause() {
            write!(f, "{}: {}", self.description(), cause)
        } else {
            f.write_str(self.description())
        }
    }
}

/// An error returned when sending input to the spawned process fails. See
/// [`Client.send()`](../expect/struct.Client.html#method.send) and
/// [`Client.send_line()`](../expect/struct.Client.html#method.send_line)
#[derive(Debug)]
pub enum SendError {
    ChanError,
}

impl Error for SendError {
    fn description(&self) -> &str {
        match *self {
            SendError::ChanError => "failed to share data between the expect session running in the event loop and the client",
        }
    }

    fn cause(&self) -> Option<&Error> {
        None
    }
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        f.write_str(self.description())
    }
}

#[derive(Debug)]
pub enum ExpectError {
    Eof,
    Timeout,
    Io(io::Error),
    ChanError,
}

impl Error for ExpectError {
    fn description(&self) -> &str {
        match *self {
            ExpectError::Eof => "met unexpected EOF while reading output",
            ExpectError::Timeout => "timeout while trying to find matches output",
            ExpectError::Io(_) => "internal IO error",
            ExpectError::ChanError => "failed to share data between the expect session running in the event loop and the client",
        }
    }

    fn cause(&self) -> Option<&Error> {
        if let ExpectError::Io(ref e) = *self {
            Some(e)
        } else {
            None
        }
    }
}

impl fmt::Display for ExpectError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        if let Some(cause) = self.cause() {
            write!(f, "{}: {}", self.description(), cause)
        } else {
            f.write_str(self.description())
        }
    }
}

/// Error returned when [`Expect.spawn()`](../expect/struct.Expect.html#method.spawn) fails.
#[derive(Debug)]
pub enum SpawnError {
    /// Failed to created a PTY for the spawned process
    Pty(io::Error),
    /// Failed to spawn the process
    SpawnCommand(io::Error),
    /// Failed to communicate with the worker that spawns should have spawned the process. This is
    /// an internal error that is not recoverable. You probably won't be able to spend any more
    /// expect session with the current handle.
    NoEventLoop,
}

impl Error for SpawnError {
    fn description(&self) -> &str {
        match *self {
            SpawnError::Pty(_) => "failed to spawn a pty",
            SpawnError::SpawnCommand(_) => "failed to spawn the command",
            SpawnError::NoEventLoop => "failed to send the spawn request to the event loop",
        }
    }

    fn cause(&self) -> Option<&Error> {
        match *self {
            SpawnError::Pty(ref e) | SpawnError::SpawnCommand(ref e) => Some(e),
            SpawnError::NoEventLoop => None,
        }
    }
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        if let Some(cause) = self.cause() {
            write!(f, "{}: {}", self.description(), cause)
        } else {
            f.write_str(self.description())
        }
    }
}
