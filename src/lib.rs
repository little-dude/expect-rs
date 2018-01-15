#[macro_use] extern crate log;
extern crate bytes;
extern crate futures;
extern crate regex;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_process;

use std::error::Error;
use std::fmt;
use std::io::{self, Cursor, Read};
use std::collections::VecDeque;
use std::process::{Command, Stdio};
use std::time::Duration;

use bytes::Buf;
use bytes::BytesMut;

use futures::{Async, Canceled, Future, Poll, Stream};
use futures::sync::{mpsc, oneshot};

use regex::Regex;

use tokio_process::{Child, ChildStderr, ChildStdin, ChildStdout, CommandExt};
use tokio_io::{codec, AsyncWrite};
use tokio_core::reactor::Handle as TokioHandle;
use tokio_core::reactor::Timeout;

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
        use Match::*;
        match (other, self) {
            (&Regex(ref r_other), &Regex(ref r)) => r_other.as_str() == r.as_str(),
            (&Utf8(ref s_other), &Utf8(ref s)) => s_other == s,
            (&Bytes(ref b_other), &Bytes(ref b)) => b_other == b,
            (&Eof, &Eof) | (&Timeout, &Timeout) => true,
            _ => false,
        }
    }
}

struct InputRequest(Vec<u8>, oneshot::Sender<()>);

struct ActiveInputRequest(Cursor<Vec<u8>>, oneshot::Sender<()>);

impl From<InputRequest> for ActiveInputRequest {
    fn from(req: InputRequest) -> Self {
        ActiveInputRequest(Cursor::new(req.0), req.1)
    }
}

pub struct Session {
    /// A tokio core handle used to spawn asynchronous tasks.
    handle: TokioHandle,

    /// process we're interacting with
    #[allow(dead_code)]
    process: Child,
    /// process stdin async writer
    stdin: ChildStdin,
    /// process stdout async reader
    stdout: ChildStdout,

    /// buffer where we store bytes read from stdout
    buffer: Vec<u8>,

    /// Receiver for the input requests coming from the expect handle.
    input_requests_rx: mpsc::UnboundedReceiver<InputRequest>,
    /// FIFO storage for the input requests. Requests are processed one after another, not
    /// concurrently.
    input_requests: VecDeque<ActiveInputRequest>,

    /// Receiver for the matching requests coming from the expect handle.
    match_requests_rx: mpsc::UnboundedReceiver<MatchRequest>,

    /// FIFO storage for the matching requests. Requests are processed one after another, not
    /// concurrently.
    match_requests: VecDeque<ActiveMatchRequest>,
}

#[derive(Debug)]
struct MatchRequest {
    /// A list of potential matches. The matches are tried sequentially.
    matches: Vec<Match>,
    /// A channel used to transmit the matching results back to the expect handle.
    response_tx: oneshot::Sender<MatchOutcome>,
    /// Optional timeout for a match to be found.
    timeout: Option<Duration>,
}

impl MatchRequest {
    fn activate(self, handle: &TokioHandle) -> Result<ActiveMatchRequest, io::Error> {
        Ok(ActiveMatchRequest {
            matches: self.matches,
            response_tx: self.response_tx,
            // FIXME: handle errors
            timeout: self.timeout
                .map(|duration| Timeout::new(duration, handle).unwrap()),
        })
    }
}

#[derive(Debug)]
struct ActiveMatchRequest {
    /// A list of potential matches. The matches are tried sequentially.
    matches: Vec<Match>,
    /// A channel used to transmit the matching results back to the expect handle.
    response_tx: oneshot::Sender<MatchOutcome>,
    /// Optional timeout for a match to be found.
    timeout: Option<Timeout>,
}

#[derive(Debug, Clone, Hash)]
pub enum MatchError {
    Eof,
    Timeout,
}

impl Error for MatchError {
    fn description(&self) -> &str {
        match *self {
            MatchError::Eof => "met unexpected EOF while reading stdout",
            MatchError::Timeout => "timeout while trying to find matches in stdout",
        }
    }

    fn cause(&self) -> Option<&Error> {
        None
    }
}

impl fmt::Display for MatchError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        f.write_str(self.description())
    }
}

type MatchOutcome = Result<(usize, Vec<u8>), MatchError>;

#[derive(Clone)]
pub struct Handle {
    match_requests_tx: mpsc::UnboundedSender<MatchRequest>,
    input_requests_tx: mpsc::UnboundedSender<InputRequest>,
}

struct LineCodec;

impl codec::Decoder for LineCodec {
    type Item = String;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<String>, io::Error> {
        if let Some(n) = buf.as_ref().iter().position(|b| *b == b'\n') {
            let line = buf.split_to(n);
            buf.split_to(1);
            return match ::std::str::from_utf8(line.as_ref()) {
                Ok(s) => Ok(Some(s.to_string())),
                Err(_) => Err(io::Error::new(io::ErrorKind::Other, "invalid string")),
            };
        }
        Ok(None)
    }
}

pub struct Stderr(codec::FramedRead<ChildStderr, LineCodec>);

impl Stderr {
    fn new(stderr: ChildStderr) -> Self {
        Stderr(codec::FramedRead::new(stderr, LineCodec {}))
    }
}

impl Stream for Stderr {
    type Item = String;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.0.poll()
    }
}

impl Handle {
    pub fn send(&self, bytes: Vec<u8>) -> Box<Future<Item = (), Error = Canceled>> {
        let handle = self.clone();
        let (response_tx, response_rx) = oneshot::channel::<()>();
        handle.input_requests_tx.unbounded_send(InputRequest(bytes, response_tx)).unwrap();
        Box::new(response_rx)
    }

    pub fn expect(&mut self, matches: Vec<Match>, timeout: Option<Duration>) -> Box<Future<Item = MatchOutcome, Error = ()>> {
        let (response_tx, response_rx) = oneshot::channel::<MatchOutcome>();
        let request = MatchRequest {
            matches,
            response_tx,
            timeout,
        };
        let handle = self.clone();
        handle.match_requests_tx.unbounded_send(request).unwrap();
        Box::new(response_rx.map_err(|_| ()))
    }
}

impl Session {
    pub fn spawn(cmd: &mut Command, handle: TokioHandle) -> Result<Handle, ()> {
        debug!("spawning new command {:?}", cmd);
        let mut process = cmd
            .stdout(Stdio::piped())
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .env("RUST_BACKTRACE", "1")
            .spawn_async(&handle.clone())
            .unwrap();

        let (input_tx, input_rx) = mpsc::unbounded::<InputRequest>();
        let (match_tx, match_rx) = mpsc::unbounded::<MatchRequest>();

        handle.clone().spawn(
            Stderr::new(process.stderr().take().unwrap()).for_each(|msg| {
                error!("spawned process stderr: {}", msg);
                Ok(())
            }).map_err(|_| ()));

        let session = Session {
            stdout: process.stdout().take().unwrap(),
            stdin: process.stdin().take().unwrap(),
            process: process,
            handle: handle.clone(),
            buffer: Vec::new(),
            input_requests_rx: input_rx,
            match_requests_rx: match_rx,
            input_requests: VecDeque::new(),
            match_requests: VecDeque::new(),
        };
        handle.spawn(session);

        Ok(Handle {
            match_requests_tx: match_tx,
            input_requests_tx: input_tx,
        })
    }

    fn poll_pending_input(req: &mut ActiveInputRequest, stdin: &mut ChildStdin) -> Poll<(), io::Error> {
        debug!("processing pending input request");
        loop {
            match stdin.write_buf(&mut req.0) {
                Ok(Async::Ready(_)) => {
                    if !req.0.has_remaining() {
                        return Ok(Async::Ready(()));
                    }
                    // FIXME: do we need to check if we wrote 0 bytes to avoid infinite looping?
                    continue;
                }
                // Cannot happen according to the docs https://docs.rs/tokio-io/0.1.4/tokio_io/trait.AsyncWrite.html
                Ok(Async::NotReady) => unreachable!(),
                Err(e) => {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        return Ok(Async::NotReady);
                    } else {
                        return Err(e);
                    }
                }
            }
        }
    }

    fn poll_pending_match(
        req: &mut ActiveMatchRequest,
        buffer: &mut Vec<u8>,
        eof: bool,
    ) -> Poll<MatchOutcome, io::Error> {
        debug!("checking pending match request: {:?}", req);

        if let Some((match_index, match_start, match_end)) = Self::try_match(req, buffer) {
            debug!("found match (mach index {}, match range: [{}, {}]", match_index, match_start, match_end);
            let match_buf = Self::extract_match(buffer, match_start, match_end);
            return Ok(Async::Ready(Ok((match_index, match_buf))));
        }

        if eof {
            // Maybe we were expecting EOF.
            if let Some(i) = req.matches.iter().position(|m| *m == Match::Eof) {
                debug!("found EOF match (mach index {})", i);
                return Ok(Async::Ready(Ok((i, buffer.clone()))));
            } else {
                debug!("found unexpected EOF");
                return Ok(Async::Ready(Err(MatchError::Eof)));
            }
        }

        if let Some(ref mut timeout) = req.timeout {
            debug!("no match found, checking whether this match request timed out already");
            match timeout.poll() {
                Ok(Async::Ready(())) => {
                    if let Some(i) = req.matches.iter().position(|m| *m == Match::Timeout) {
                        debug!("found timeout match (match index {})", i);
                        return Ok(Async::Ready(Ok((i, buffer.clone()))));
                    } else {
                        debug!("found unexpected timeout");
                        return Ok(Async::Ready(Err(MatchError::Timeout)));
                    }
                }
                Ok(Async::NotReady) => {
                    debug!("the match request did not time out yet");
                    return Ok(Async::NotReady);
                }
                // if the timeout future fails, just return the error, because I don't really know
                // what to do at this point.
                Err(e) => {
                    error!("unexpected error while polling timeout future: {}", e);
                    return Err(e);
                }
            }
        }

        // if there's not timeout, just return that we're not ready yet
        Ok(Async::NotReady)
    }

    fn read_stdout(&mut self) -> Result<usize, io::Error> {
        debug!("reading from stdout");
        let mut buf = [0; 4096];
        let mut size = 0;
        loop {
            // read is supposed to be sync, but since ChildStdout implement AsyncRead, it's async.
            // See the doc about that: https://docs.rs/tokio-io/0.1.2/tokio_io/trait.AsyncRead.html
            match self.stdout.read(&mut buf[..]) {
                Ok(i) => {
                    if i == 0 {
                        warn!("met EOF while reading stdout");
                        return Err(io::ErrorKind::UnexpectedEof.into());
                    }
                    size += i;
                    self.buffer.extend_from_slice(&buf[..i]);
                    continue;
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        debug!("done reading from stdout");
                        debug!("buffer so far: {}", String::from_utf8_lossy(&self.buffer));
                        return Ok(size);
                    }
                    return Err(e);
                }
            }
        }
    }

    // FIXME: return proper error
    fn get_match_requests(&mut self) -> Result<(), ()> {
        let tokio_handle = self.handle.clone();
        loop {
            match self.match_requests_rx.poll() {
                Ok(Async::Ready(Some(req))) => {
                    // FIXME: error handling
                    self.match_requests
                        .push_back(req.activate(&tokio_handle).unwrap())
                }
                // the channel is closed
                Ok(Async::Ready(None)) => {
                    warn!("cannot receive match requests anymore: channel is closed");
                    return Err(());
                }
                Ok(Async::NotReady) => return Ok(()),
                Err(e) => {
                    error!("failed to read from match requests channel");
                    return Err(e);
                }
            }
        }
    }

    fn get_input_requests(&mut self) -> Result<(), ()> {
        debug!("getting input requests from handle");
        loop {
            match self.input_requests_rx.poll() {
                Ok(Async::Ready(Some(req))) => {
                    self.input_requests.push_back(req.into());
                    debug!("got input request");
                }
                Ok(Async::Ready(None)) => {
                    warn!("cannot receive input requests anymore: channel is closed");
                    return Err(());
                }
                Ok(Async::NotReady) => return Ok(()),
                Err(e) => {
                    error!("failed to read from input requests channel");
                    return Err(e);
                }
            }
        }
    }

    fn try_match(req: &mut ActiveMatchRequest, buffer: &[u8]) -> Option<(usize, usize, usize)> {
        let string = String::from_utf8_lossy(buffer);
        for (i, m) in req.matches.iter().enumerate() {
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

    fn extract_match(buffer: &mut Vec<u8>, start: usize, end: usize) -> Vec<u8> {
        let matched = buffer.split_off(start);
        let _ = buffer.split_off(end - start);
        matched
    }

    fn process_input(&mut self) {
        debug!("processing input requests");
        let Session {
            ref mut input_requests,
            ref mut stdin,
            ..
        } = *self;
        let mut n_requests_processed = 0;
        while let Some(req) = input_requests.get_mut(n_requests_processed) {
            match Self::poll_pending_input(req, stdin) {
                Ok(Async::Ready(())) => {
                    debug!("processed input request");
                    n_requests_processed += 1;
                }
                Ok(Async::NotReady) => break,
                Err(e) => panic!("failed to process input request: {}", e),
            }
        }
        for _i in 0..n_requests_processed {
            let req = input_requests.pop_front().unwrap();
            req.1.send(()).unwrap();
        }
    }

    fn process_matches(&mut self) -> Result<(), io::Error> {
        let mut eof = false;
        match self.read_stdout() {
            Ok(i) => {
                debug!("read {} bytes from stdout", i);
            }
            Err(e) => {
                if e.kind() == io::ErrorKind::UnexpectedEof {
                    debug!("EOF in stdout");
                    eof = true;
                } else {
                    return Err(e);
                }
            }
        }

        let Session {
            ref mut match_requests,
            ref mut buffer,
            ..
        } = *self;

        let mut results = vec![];
        while let Some(req) = match_requests.get_mut(results.len()) {
            match Self::poll_pending_match(req, buffer, eof) {
                Ok(Async::Ready(result)) => results.push(result),
                Ok(Async::NotReady) => break,
                Err(e) => panic!("error while processing match request {:?}: {}", req, e),
            }
        }
        for res in results.drain(..) {
            let req = match_requests.pop_front().unwrap();
            req.response_tx.send(res).unwrap();
        }

        Ok(())
    }
}

impl Future for Session {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.get_input_requests().unwrap();
        self.get_match_requests().unwrap();
        self.process_input();
        self.process_matches().unwrap();



        Ok(Async::NotReady)
    }
}