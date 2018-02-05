use std::error::Error;
use std::fmt;
use std::io;

#[derive(Debug)]
pub enum InitError {
    Reactor(io::Error),
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
            InitError::Reactor(ref e) => Some(e),
            InitError::SpawnWorker(ref e) => Some(e),
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
    /// No match found, and EOF reached while reading from the PTY
    Eof,
    /// No match found, and timeout reached for the given match request
    Timeout,
    ///
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

#[derive(Debug)]
pub enum SpawnError {
    Pty(io::Error),
    SpawnCommand(io::Error),
    Internal,
}

impl Error for SpawnError {
    fn description(&self) -> &str {
        match *self {
            SpawnError::Pty(_) => "failed to spawn a pty",
            SpawnError::SpawnCommand(_) => "failed to spawn the command",
            SpawnError::Internal => "an error occured within the expect event loop",
        }
    }

    fn cause(&self) -> Option<&Error> {
        match *self {
            SpawnError::Pty(ref e) => Some(e),
            SpawnError::SpawnCommand(ref e) => Some(e),
            SpawnError::Internal => None,
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
