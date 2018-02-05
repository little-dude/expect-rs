use std::error::Error;
use std::fmt;
use std::time::Duration;

use {core, Match, MatchRequest, SendError};
use regex::Regex;

pub struct Client {
    inner: core::Client,
    mode: core::MatchMode,
    timeout: Option<Duration>,
}

impl From<core::Client> for Client {
    fn from(client: core::Client) -> Self {
        Client {
            inner: client,
            mode: core::MatchMode::Line,
            timeout: Some(Duration::from_millis(5000)),
        }
    }
}

impl Client {
    pub fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<(), SendError> {
        self.inner.send(bytes)
    }

    pub fn send(&mut self, string: String) -> Result<(), SendError> {
        self.inner.send(string.as_bytes().to_vec())
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
        let res = self.expect(vec![core::Match::Utf8(string)])?;
        assert_eq!(res.0, 0);
        Ok(res.1)
    }

    pub fn match_regex(&mut self, re: Regex) -> Result<String, ExpectError> {
        let res = self.expect(vec![core::Match::Regex(re)])?;
        assert_eq!(res.0, 0);
        Ok(res.1)
    }

    pub fn match_timeout(&mut self, _duration: Duration) -> Result<String, ExpectError> {
        // TODO: actually set the duration
        let res = self.expect(vec![core::Match::Timeout])?;
        assert_eq!(res.0, 0);
        Ok(res.1)
    }

    pub fn match_eof(&mut self) -> Result<String, ExpectError> {
        let res = self.expect(vec![core::Match::Eof])?;
        assert_eq!(res.0, 0);
        Ok(res.1)
    }

    pub fn expect(&mut self, matches: Vec<Match>) -> Result<(usize, String), ExpectError> {
        let request = self.get_request().set_matches(matches);
        let mut res = self.inner.expect(request)?;
        Ok((res.0, String::from_utf8_lossy(res.1.as_mut()).into_owned()))
    }

    fn get_request(&self) -> MatchRequest {
        MatchRequest::new()
            .set_opt_timeout(self.timeout.clone())
            .set_mode(self.mode)
    }
}

#[derive(Debug)]
pub enum ExpectError {
    Internal(String),
    Eof,
    Timeout,
}

impl ExpectError {
    pub fn is_eof(&self) -> bool {
        match *self {
            ExpectError::Eof => true,
            _ => false,
        }
    }

    pub fn is_timeout(&self) -> bool {
        match *self {
            ExpectError::Timeout => true,
            _ => false,
        }
    }

    pub fn is_internal(&self) -> bool {
        match *self {
            ExpectError::Internal(_) => true,
            _ => false,
        }
    }
}

impl From<core::ExpectError> for ExpectError {
    fn from(e: core::ExpectError) -> Self {
        match e {
            core::ExpectError::Eof => ExpectError::Eof,
            core::ExpectError::Timeout => ExpectError::Timeout,
            e => ExpectError::Internal(format!("Internal error: {}", e)),
        }
    }
}

impl Error for ExpectError {
    fn description(&self) -> &str {
        match *self {
            ExpectError::Internal(_) => "an internal error occured",
            ExpectError::Eof => "EOF while reading process output",
            ExpectError::Timeout => "time out while trying to find a match",
        }
    }

    fn cause(&self) -> Option<&Error> {
        None
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
