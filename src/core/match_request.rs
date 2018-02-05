use std::io;
use std::time::Duration;

use futures::{Async, Future};
use futures::sync::oneshot;
use regex::Regex;
use tokio_core::reactor::Handle;
use tokio_core::reactor::Timeout as TokioTimeout;

use super::errors::ExpectError;

#[derive(Debug, Clone)]
pub enum Match {
    /// Match a regular expression
    Regex(Regex),
    /// Match a specific utf-8 string
    Utf8(String),
    /// Match a specific sequence of bytes
    Bytes(Vec<u8>),
    /// Expect the process's stdout to close, resulting in EOF
    Eof,
    /// Expect a timeout
    Timeout,
}

// Regex does not implement PartialEq. See https://github.com/rust-lang/regex/issues/178
impl PartialEq for Match {
    fn eq(&self, other: &Self) -> bool {
        match (other, self) {
            (&Match::Regex(ref r_other), &Match::Regex(ref r)) => r_other.as_str() == r.as_str(),
            (&Match::Utf8(ref s_other), &Match::Utf8(ref s)) => s_other == s,
            (&Match::Bytes(ref b_other), &Match::Bytes(ref b)) => b_other == b,
            (&Match::Eof, &Match::Eof) | (&Match::Timeout, &Match::Timeout) => true,
            _ => false,
        }
    }
}

#[derive(Debug)]
pub struct InputRequest(pub Vec<u8>, pub oneshot::Sender<()>);

#[derive(Copy, Debug, Clone, Eq, PartialEq)]
pub enum MatchMode {
    Raw,
    Line,
}

#[derive(Debug)]
pub struct MatchRequest {
    /// A list of potential matches. The matches are tried sequentially.
    matches: Vec<Match>,
    /// A channel used to transmit the matching results back to the expect handle.
    response_tx: oneshot::Sender<MatchOutcome>,
    /// Optional timeout for a match to be found.
    timeout: Timeout,
    mode: MatchMode,
}

#[derive(Debug)]
enum Timeout {
    Inactive(Duration),
    Active(TokioTimeout),
    Finished,
    None,
}

impl Timeout {
    fn activate(&mut self, handle: &Handle) -> Result<(), io::Error> {
        let active_timeout = if let Timeout::Inactive(ref duration) = *self {
            Timeout::Active(TokioTimeout::new(duration.clone(), handle)?)
        } else {
            return Ok(());
        };
        *self = active_timeout;
        Ok(())
    }

    /// Return `true` if the request timed out already, `false` otherwise.
    fn timed_out(&mut self) -> Result<bool, io::Error> {
        match *self {
            Timeout::Active(ref mut timeout) => {
                if let Async::NotReady = timeout.poll()? {
                    return Ok(false);
                }
            }
            Timeout::Finished => return Ok(true),
            Timeout::Inactive(_) | Timeout::None => return Ok(false),
        }
        // If we're here, it means the timeout task finished
        *self = Timeout::Finished;
        Ok(true)
    }
}

impl From<Option<Duration>> for Timeout {
    fn from(duration: Option<Duration>) -> Self {
        if let Some(duration) = duration {
            Timeout::Inactive(duration)
        } else {
            Timeout::None
        }
    }
}

impl MatchRequest {
    /// Start the timeout future for the match request, effectively "activating" the match request.
    pub fn activate(&mut self, handle: &Handle) -> Result<(), io::Error> {
        self.timeout.activate(handle)
    }

    pub fn timed_out(&mut self) -> Result<bool, io::Error> {
        self.timeout.timed_out()
    }

    fn find_buffer_match(&self, buffer: &[u8]) -> Option<(usize, usize, usize)> {
        match self.mode {
            MatchMode::Line => self.match_lines(buffer),
            MatchMode::Raw => self.match_raw(buffer),
        }
    }

    pub fn match_buffer(&self, buffer: &mut Vec<u8>) -> Option<(usize, Vec<u8>)> {
        self.find_buffer_match(buffer)
            .and_then(|(index, start, end)| {
                let match_buf = extract_match(buffer, start, end);
                Some((index, match_buf))
            })
    }

    pub fn get_timeout_match_index(&self) -> Option<usize> {
        self.matches.iter().position(|m| *m == Match::Timeout)
    }

    pub fn get_eof_match_index(&self) -> Option<usize> {
        self.matches.iter().position(|m| *m == Match::Eof)
    }

    fn match_lines(&self, buffer: &[u8]) -> Option<(usize, usize, usize)> {
        let lines = get_lines(buffer, LineMode::Full);
        for (i, m) in self.matches.iter().enumerate() {
            let mut offset = 0;
            debug!("trying to find match for {:?} (index {})", m, i);
            for line in &lines {
                match *m {
                    Match::Regex(ref re) => {
                        if let Some(position) = re.find(&line) {
                            return Some((i, offset + position.start(), offset + position.end()));
                        }
                    }
                    Match::Utf8(ref s) => {
                        if let Some(start) = line.find(s) {
                            return Some((i, offset + start, offset + start + s.len()));
                        }
                    }
                    Match::Bytes(ref _bytes) => unimplemented!(),
                    Match::Eof | Match::Timeout => {}
                }
                offset += line.as_bytes().len()
            }
        }
        None
    }

    fn match_raw(&self, buffer: &[u8]) -> Option<(usize, usize, usize)> {
        let string = String::from_utf8_lossy(buffer);
        for (i, m) in self.matches.iter().enumerate() {
            debug!("trying to find match for {:?} (index {})", m, i);
            match *m {
                Match::Regex(ref re) => {
                    if let Some(position) = re.find(&string) {
                        return Some((i, position.start(), position.end()));
                    }
                }
                Match::Utf8(ref s) => {
                    if let Some(start) = string.find(s) {
                        return Some((i, start, start + s.len()));
                    }
                }
                Match::Bytes(ref _bytes) => unimplemented!(),
                Match::Eof | Match::Timeout => continue,
            }
        }
        None
    }

    pub fn send_outcome(self, outcome: MatchOutcome) {
        if let Err(_) = self.response_tx.send(outcome) {
            // Ignore the error: if the receiver was dropped, it does not care about the outcome
            // anyway.
            error!("Cannot send match outcome to client: the receiver was dropped already");
        }
    }
}

pub struct MatchRequestBuilder(MatchRequest, oneshot::Receiver<MatchOutcome>);

impl MatchRequestBuilder {
    pub fn new() -> Self {
        let (response_tx, response_rx) = oneshot::channel::<MatchOutcome>();
        let req = MatchRequest {
            matches: Vec::new(),
            response_tx,
            timeout: Timeout::None,
            mode: MatchMode::Line,
        };
        MatchRequestBuilder(req, response_rx)
    }

    pub(crate) fn set_mode(mut self, mode: MatchMode) -> Self {
        self.0.mode = mode;
        self
    }

    pub(crate) fn set_opt_timeout(self, timeout: Option<Duration>) -> Self {
        if let Some(duration) = timeout {
            self.set_timeout(duration)
        } else {
            self.unset_timeout()
        }
    }

    pub fn set_line_mode(mut self) -> Self {
        self.0.mode = MatchMode::Line;
        self
    }

    pub fn set_raw_mode(mut self) -> Self {
        self.0.mode = MatchMode::Raw;
        self
    }

    pub fn set_timeout(mut self, duration: Duration) -> Self {
        self.0.timeout = Timeout::Inactive(duration);
        self
    }

    pub fn unset_timeout(mut self) -> Self {
        self.0.timeout = Timeout::None;
        self
    }

    pub fn set_matches(mut self, matches: Vec<Match>) -> Self {
        self.0.matches = matches;
        self
    }

    pub(crate) fn build(self) -> (MatchRequest, oneshot::Receiver<MatchOutcome>) {
        (self.0, self.1)
    }
}

pub type MatchOutcome = Result<(usize, Vec<u8>), ExpectError>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LineMode {
    Full,
    Trimmed,
}

fn get_lines(buf: &[u8], mode: LineMode) -> Vec<String> {
    let mut lines = Vec::new();
    let mut prev_idx = 0;
    let mut new_idx;
    while prev_idx < buf.len() {
        if let Some(offset) = buf[prev_idx..].iter().position(|b| *b == b'\n') {
            new_idx = prev_idx + offset;
            if mode == LineMode::Trimmed {
                lines.push(String::from_utf8_lossy(&buf[prev_idx..new_idx + 1]).into_owned());
            }
            if buf.len() < new_idx + 1 && buf[new_idx + 1] == b'\r' {
                new_idx += 1;
            }
            if mode == LineMode::Full {
                lines.push(String::from_utf8_lossy(&buf[prev_idx..new_idx + 1]).into_owned());
            }
            prev_idx = new_idx + 1;
        } else {
            lines.push(String::from_utf8_lossy(&buf[prev_idx..]).into_owned());
            break;
        }
    }
    return lines;
}

fn extract_match(buffer: &mut Vec<u8>, start: usize, end: usize) -> Vec<u8> {
    let new_buf = buffer.split_off(end);
    let _matched = buffer.split_off(start);
    // buffer now contains what we want to return
    let ret = buffer.clone();
    *buffer = new_buf;
    ret
}
