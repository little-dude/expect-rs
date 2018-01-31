extern crate futures;
extern crate libc;
#[macro_use]
extern crate log;
extern crate mio;
extern crate regex;
extern crate tokio_core;
extern crate tokio_io;
extern crate tty;

mod pty;
mod core;

pub use core::Match;
use regex::Regex;
use std::process::Command;
use std::fmt;
use std::borrow::Cow;

use std::time::Duration;
use std::error::Error;

#[derive(Debug)]
pub struct InitError(core::InitError);

impl Error for InitError {
    fn description(&self) -> &str {
        self.0.description()
    }
    fn cause(&self) -> Option<&Error> {
        None
    }
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        self.0.fmt(f)
    }
}

#[derive(Debug)]
pub struct SendError(core::InternalError);

impl Error for SendError {
    fn description(&self) -> &str {
        self.0.description()
    }
    fn cause(&self) -> Option<&Error> {
        None
    }
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        self.0.fmt(f)
    }
}

pub struct Client {
    inner: core::Client,
    mode: core::MatchMode,
    timeout: Option<Duration>,
}

impl Client {
    pub fn spawn(cmd: Command) -> Result<Self, InitError> {
        let inner = core::Client::spawn(cmd).map_err(|e| InitError(e))?;
        Ok(Client {
            inner: inner,
            mode: core::MatchMode::Line,
            timeout: Some(Duration::from_millis(5000)),
        })
    }

    pub fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<(), SendError> {
        self.inner.send(bytes).map_err(|e| SendError(e))
    }

    pub fn send(&mut self, string: String) -> Result<(), SendError> {
        self.inner
            .send(string.as_bytes().to_vec())
            .map_err(|e| SendError(e))
    }

    pub fn send_line(&mut self, string: String) -> Result<(), SendError> {
        let mut string = string;
        string.push('\n');
        self.send(string)
    }

    pub fn set_line_mode(&mut self) {
        self.mode = core::MatchMode::Line;
    }

    pub fn set_raw_mode(&mut self) {
        self.mode = core::MatchMode::Raw;
    }

    pub fn set_timeout(&mut self, timeout: Option<Duration>) {
        self.timeout = timeout;
    }

    pub fn match_string(&mut self, string: String) -> Result<String, ExpectError> {
        let match_req = vec![core::Match::Utf8(string)];
        let timeout = self.timeout.clone();
        let mut res = self.inner.expect(match_req, timeout, self.mode)??;
        assert_eq!(res.0, 0);
        Ok(String::from_utf8_lossy(res.1.as_mut()).into_owned())
    }

    pub fn match_regex(&mut self, re: Regex) -> Result<String, ExpectError> {
        let match_req = vec![core::Match::Regex(re)];
        let mut res = self.inner
            .expect(match_req, self.timeout.clone(), self.mode)??;
        assert_eq!(res.0, 0);
        Ok(String::from_utf8_lossy(res.1.as_mut()).into_owned())
    }

    pub fn match_timeout(&mut self, duration: Duration) -> Result<String, ExpectError> {
        let match_req = vec![core::Match::Timeout];
        let mut res = self.inner.expect(match_req, Some(duration), self.mode)??;
        assert_eq!(res.0, 0);
        Ok(String::from_utf8_lossy(res.1.as_mut()).into_owned())
    }

    pub fn match_eof(&mut self) -> Result<String, ExpectError> {
        let mut res = self.inner
            .expect(vec![core::Match::Eof], self.timeout.clone(), self.mode)??;
        assert_eq!(res.0, 0);
        Ok(String::from_utf8_lossy(res.1.as_mut()).into_owned())
    }

    pub fn expect(&mut self, matches: Vec<Match>) -> Result<(usize, String), ExpectError> {
        let mut res = self.inner
            .expect(matches, self.timeout.clone(), self.mode)??;
        Ok((res.0, String::from_utf8_lossy(res.1.as_mut()).into_owned()))
    }
}

#[derive(Debug)]
pub enum ExpectError {
    Exited(core::InternalError),
    Eof,
    Timeout,
}

impl ExpectError {
    fn is_eof(&self) -> bool {
        match *self {
            ExpectError::Eof => true,
            _ => false,
        }
    }

    fn is_timeout(&self) -> bool {
        match *self {
            ExpectError::Timeout => true,
            _ => false,
        }
    }

    fn is_exited(&self) -> bool {
        match *self {
            ExpectError::Exited(_) => true,
            _ => false,
        }
    }
}

impl From<core::ExpectError> for ExpectError {
    fn from(e: core::ExpectError) -> Self {
        match e {
            core::ExpectError::Eof => ExpectError::Eof,
            core::ExpectError::Timeout => ExpectError::Timeout,
        }
    }
}
impl From<core::InternalError> for ExpectError {
    fn from(e: core::InternalError) -> Self {
        ExpectError::Exited(e)
    }
}

impl Error for ExpectError {
    fn description(&self) -> &str {
        match *self {
            ExpectError::Exited(_) => "the expect session finished",
            ExpectError::Eof => "EOF while reading process output",
            ExpectError::Timeout => "time out while trying to find a match",
        }
    }

    fn cause(&self) -> Option<&Error> {
        match *self {
            ExpectError::Exited(ref e) => Some(e),
            _ => None,
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
