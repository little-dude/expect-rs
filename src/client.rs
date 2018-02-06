use std::error::Error;
use std::fmt;
use std::time::Duration;

use {core, Match, MatchRequest, SendError};
use regex::Regex;

/// A client for a specific expect session. It can be used to send input and look for specific
/// output from the spawned process.
///
/// ## Types of matches
///
/// It is possible to match the output using several patterns:
///
/// - a string (see [`match_string()`](struct.Client.html#method.match_string))
/// - a regular expression (see [`match_regex()`](struct.Client.html#method.match_regex))
/// - a specific byte sequence (not implemented yet)
/// - matching EOF in the spawned process PTY. This usually mean the process exited (see
///   [`match_eof()`](struct.Client.html#method.match_eof))
/// - matching timeouts. It means matching all the output the spawned process produced within a
///   certain time (see [`match_timeout()`](struct.Client.html#method.match_timeout))
///
/// ## Match modes
///
/// There are two ways for matching output:
///
/// - the "line" mode: the output is splitted into lines, and each line is tried until a match is
///   found
/// - the "raw" mode: the output is kept as is, and newlines (`\n`) and carriage returns (`\r`) can
///   be part of the match pattern. This is useful to match multiline prompts for example.
///
/// The default mode is to match against lines. Use
/// [`set_line_mode`](struct.Client.html#method.set_line_mode) and
/// [`set_raw_mode`](struct.Client.html#method.set_raw_mode) to set a specific mode.
///
/// ## Timeouts
///
/// By default, if no match is found, all `match_XXX()` methods return `ExpectError::Timeout` after
/// 10 seconds. This timeout is configurable and can even be completely removed, in which case
/// `match_XXX()` methods will block until a match is found. See [`set_timeout()`](struct.Client.html#method.set_timeout).
///
/// ## Specifying multiple matches
///
/// This feature is inpired by python's implementation of expect
/// [`pexpect`](http://pexpect.readthedocs.io/). With `pexpect` it is possible to specify multiple
/// possible match patterns, and take actions based on which one matched first:
///
/// ```python,no_run
/// child.expect('password:')
/// child.sendline(my_secret_password)
/// # We expect any of these three patterns...
/// i = child.expect (['Permission denied', 'Terminal type', '[#\$] '])
/// if i==0:
///     print('Permission denied on host. Can\'t login')
///     child.kill(0)
/// elif i==1:
///     print('Login OK... need to send terminal type.')
///     child.sendline('vt100')
///     child.expect('[#\$] ')
/// elif i==2:
///     print('Login OK.')
///     print('Shell command prompt', child.after)
/// ```
///
/// I liked and used this feature a lot so this crates provides something similar via
/// [`expect()`](struct.Client.html#method.expect), which accept a list of match requests (see
/// [`MatchRequest`](struct.MatchRequest.html), and returns the index of the first one that was
/// matched along with the buffer that precedes the match.
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
    /// Send the given byte sequence to the spawned process.
    pub fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<(), SendError> {
        self.inner.send(bytes)
    }

    /// Send the given string to the spawned process.
    pub fn send(&mut self, string: &str) -> Result<(), SendError> {
        self.inner.send(string.as_bytes().to_vec())
    }

    /// Send the given string to the spawned process followed by the newline character (`\n`).
    pub fn send_line(&mut self, string: String) -> Result<(), SendError> {
        let mut string = string;
        string.push('\n');
        self.send(&string)
    }

    /// Set the "match" mode to "line" for this expect session (this is the default).
    pub fn set_line_mode(&mut self) {
        self.mode = core::MatchMode::Line;
    }

    /// Set the "match" mode to "raw" for this expect session.
    pub fn set_raw_mode(&mut self) {
        self.mode = core::MatchMode::Raw;
    }

    /// Set a custom timeout for all match requests. By default, it is set to 10 seconds.
    pub fn set_timeout(&mut self, timeout: Option<Duration>) {
        self.timeout = timeout;
    }

    /// Match the given string in the spawned process output and return the output that precedes
    /// the match.
    pub fn match_string(&mut self, string: String) -> Result<String, ExpectError> {
        let res = self.expect(vec![core::Match::Utf8(string)])?;
        assert_eq!(res.0, 0);
        Ok(res.1)
    }

    /// Match the given pattern in the spawned process output and return the output that precedes
    /// the match.
    pub fn match_regex(&mut self, re: Regex) -> Result<String, ExpectError> {
        let res = self.expect(vec![core::Match::Regex(re)])?;
        assert_eq!(res.0, 0);
        Ok(res.1)
    }

    /// Match all the spawned process output for the given duration and return the output that
    /// precedes the match, _i.e._ all the output produced by the spawned process before the time
    /// out occured.
    pub fn match_timeout(&mut self, _duration: Duration) -> Result<String, ExpectError> {
        // TODO: actually set the duration
        let res = self.expect(vec![core::Match::Timeout])?;
        assert_eq!(res.0, 0);
        Ok(res.1)
    }

    /// Match EOF in the spawned process PTY and return all the spawned process output up to EOF.
    /// That usually means the process exited.
    pub fn match_eof(&mut self) -> Result<String, ExpectError> {
        let res = self.expect(vec![core::Match::Eof])?;
        assert_eq!(res.0, 0);
        Ok(res.1)
    }

    /// Try to match against a list of possible match patterns, and return the index of the pattern
    /// that matched first, along with the output that precedes the match.
    pub fn expect(&mut self, matches: Vec<Match>) -> Result<(usize, String), ExpectError> {
        let request = self.get_request().set_matches(matches);
        let mut res = self.inner.expect(request)?;
        Ok((res.0, String::from_utf8_lossy(res.1.as_mut()).into_owned()))
    }

    fn get_request(&self) -> MatchRequest {
        MatchRequest::new()
            .set_opt_timeout(self.timeout)
            .set_mode(self.mode)
    }
}

/// An error returned when failing to match output from the spawned process.
#[derive(Debug)]
pub enum ExpectError {
    /// An internal error occured. These errors are usually not recoverable and mean the expect
    /// session is doomed. Subsequent call to [`Client.send()`](struct.Client.html#method.send) and
    /// [`Client.expect()`](struct.Client.html#method.expect) will likely fail.
    Internal(String),
    /// No match found, and EOF reached while reading from the PTY. That usually means the spawned
    /// process exited.
    Eof,
    /// Failed to find a match in time.
    Timeout,
}

impl ExpectError {
    /// Return `true` if this error is [`ExpectError::Eof`](enum.ExpectError.html#variant.Eof)
    pub fn is_eof(&self) -> bool {
        match *self {
            ExpectError::Eof => true,
            _ => false,
        }
    }

    /// Return `true` if this error is
    /// [`ExpectError::Timeout`](enum.ExpectError.html#variant.Timeout)
    pub fn is_timeout(&self) -> bool {
        match *self {
            ExpectError::Timeout => true,
            _ => false,
        }
    }

    /// Return `true` if this error is
    /// [`ExpectError::Internal`](enum.ExpectError.html#variant.Internal)
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
