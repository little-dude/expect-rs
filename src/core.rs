use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
use std::collections::VecDeque;
use std::process::Command;
use std::thread;
use std::time::Duration;

use futures::{Async, Canceled, Future, Poll, Stream};
use futures::sync::{mpsc, oneshot};
use regex::Regex;
use tokio_core::reactor::{Core, Handle, Timeout};
use pty::*;

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
struct InputRequest(Vec<u8>, oneshot::Sender<()>);

#[derive(Copy, Debug, Clone, Eq, PartialEq)]
pub enum MatchMode {
    Raw,
    Line,
}

#[derive(Debug)]
struct MatchRequest {
    /// A list of potential matches. The matches are tried sequentially.
    matches: Vec<Match>,
    /// A channel used to transmit the matching results back to the expect handle.
    response_tx: oneshot::Sender<MatchOutcome>,
    /// Optional timeout for a match to be found.
    timeout: Option<Duration>,
    mode: MatchMode,
}

impl MatchRequest {
    /// Start the timeout future for the match request, effectively "activating" the match request.
    fn activate(self, handle: &Handle) -> Result<ActiveMatchRequest, InternalError> {
        let timeout = if let Some(duration) = self.timeout {
            Some(Timeout::new(duration, handle).map_err(|e| InternalError::SpawnTimeout(e))?)
        } else {
            None
        };

        Ok(ActiveMatchRequest {
            matches: self.matches,
            response_tx: self.response_tx,
            timeout: timeout,
            mode: self.mode,
        })
    }
}

/// An active match request is a match request which timeout future started running.
#[derive(Debug)]
struct ActiveMatchRequest {
    /// A list of potential matches. The matches are tried sequentially.
    matches: Vec<Match>,
    /// A channel used to transmit the matching results back to the expect handle.
    response_tx: oneshot::Sender<MatchOutcome>,
    /// Optional timeout for a match to be found.
    timeout: Option<Timeout>,
    mode: MatchMode,
}

#[derive(Debug, Clone, Hash)]
pub enum ExpectError {
    /// No match found, and EOF reached while reading from the PTY
    Eof,
    /// No match found, and timeout reached for the given match request
    Timeout,
}

impl Error for ExpectError {
    fn description(&self) -> &str {
        match *self {
            ExpectError::Eof => "met unexpected EOF while reading output",
            ExpectError::Timeout => "timeout while trying to find matches output",
        }
    }

    fn cause(&self) -> Option<&Error> {
        None
    }
}

impl fmt::Display for ExpectError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        f.write_str(self.description())
    }
}

type MatchOutcome = Result<(usize, Vec<u8>), ExpectError>;

#[derive(Debug)]
pub struct Client {
    /// Channel to send match requests to the expect session
    match_requests_tx: mpsc::UnboundedSender<MatchRequest>,
    /// Channel to send input requests to the expect session
    input_requests_tx: mpsc::UnboundedSender<InputRequest>,
    /// Handle to the thread where the expect session is running
    thread: Option<thread::JoinHandle<()>>,
    /// Channel to send a shutdown signal to the expect session
    shutdown_tx: Option<oneshot::Sender<()>>,
    ///
    expect_worker_err_rx: Option<oneshot::Receiver<InternalError>>,
}

impl Client {
    pub fn spawn(cmd: Command) -> Result<Client, InitError> {
        debug!("spawning new command {:?}", cmd);
        let (input_tx, input_rx) = mpsc::unbounded::<InputRequest>();
        let (match_tx, match_rx) = mpsc::unbounded::<MatchRequest>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let (init_res_tx, init_res_rx) = oneshot::channel::<Result<(), InitError>>();
        let (expect_worker_err_tx, expect_worker_err_rx) = oneshot::channel::<InternalError>();

        // spawn the core future in a separate thread.
        //
        // it's bad practice to spawn our own core but otherwise, it's complicated to provide a
        // synchronous client, since calling `wait()` blocks the current thread, preventing the
        // event loop from making progress... But running the event loop in a separate thread, we
        // can call `wait()` in the client.
        let thread = thread::Builder::new()
            .name("expect-worker".into())
            .spawn(move || {
                let mut core = match Core::new() {
                    Ok(core) => core,
                    Err(e) => {
                        // this cannot fail as long as the receiver has not been dropped
                        init_res_tx.send(Err(InitError::Reactor(e))).unwrap();
                        return;
                    }
                };

                let mut pty = match Pty::new::<::std::fs::File>(None, &core.handle()) {
                    Ok(pty) => pty,
                    Err(e) => {
                        // this cannot fail as long as the receiver has not been dropped
                        init_res_tx.send(Err(InitError::Pty(e))).unwrap();
                        return;
                    }
                };

                // FIXME: keep the child around, we'll need to call waitpid other it'll become a
                // zombie.
                let child = match pty.spawn(cmd) {
                    Ok(child) => child,
                    Err(e) => {
                        // this cannot fail as long as the receiver has not been dropped
                        init_res_tx.send(Err(InitError::SpawnCommand(e))).unwrap();
                        return;
                    }
                };

                // this cannot fail as long as the receiver has not been dropped
                init_res_tx.send(Ok(())).unwrap();

                let session = EventLoop {
                    pty: pty,
                    handle: core.handle(),
                    buffer: Vec::new(),
                    input_requests_rx: input_rx,
                    match_requests_rx: match_rx,
                    input_requests: VecDeque::new(),
                    match_requests: VecDeque::new(),
                    shutdown_rx: shutdown_rx,
                };

                if let Err(e) = core.run(session) {
                    error!("expect worker terminated with an error: {}", e);
                    // The client should not have been dropped yet, since given our Drop
                    // implementation, client can only be dropped after it joined this thread.
                    expect_worker_err_tx.send(e)
                                   .expect("expect worker: failed to send error to main thread");
                }
            })
            .map_err(|e| InitError::SpawnWorker(e))?;

        // This should not happen. I see no reason why the receiver would get Err(Cancel) here.
        init_res_rx
            .wait()
            .expect("failed to retrieve the init outcome");

        Ok(Client {
            match_requests_tx: match_tx.clone(),
            input_requests_tx: input_tx.clone(),
            thread: Some(thread),
            shutdown_tx: Some(shutdown_tx),
            expect_worker_err_rx: Some(expect_worker_err_rx),
        })
    }

    pub fn send(&mut self, bytes: Vec<u8>) -> Result<(), InternalError> {
        let (response_tx, response_rx) = oneshot::channel::<()>();
        match self.input_requests_tx
            .unbounded_send(InputRequest(bytes, response_tx))
        {
            Ok(()) => response_rx.wait().or_else(|_| {
                error!("failed to receive response from the expect worker");
                // The sender has been dropped, meaning the worker failed, so we try to retrieve
                // the error, and if we can't find one we just return a generic ChanError.
                Err(self.get_worker_err().unwrap_or(InternalError::ChanError))
            }),
            Err(_) => {
                error!("failed to send input request to the expect worker");
                // The sender has been dropped, meaning the worker failed, so we try to retrieve
                // the error, and if we can't find one we just return a generic ChanError.
                Err(self.get_worker_err().unwrap_or(InternalError::ChanError))
            }
        }
    }

    pub fn expect(
        &mut self,
        matches: Vec<Match>,
        timeout: Option<Duration>,
        mode: MatchMode,
    ) -> Result<ExpectResult, InternalError> {
        let (response_tx, response_rx) = oneshot::channel::<MatchOutcome>();
        let request = MatchRequest {
            matches,
            response_tx,
            timeout,
            mode,
        };
        match self.match_requests_tx.unbounded_send(request) {
            Ok(()) => response_rx.wait().or_else(|_| {
                error!("failed to receive response from the expect worker");
                // The sender has been dropped, meaning the worker failed, so we try to retrieve
                // the error, and if we can't find one we just return a generic ChanError.
                Err(self.get_worker_err().unwrap_or(InternalError::ChanError))
            }),
            Err(_) => {
                error!("failed to send match request to the expect worker");
                // The sender has been dropped, meaning the worker failed, so we try to retrieve
                // the error, and if we can't find one we just return a generic ChanError.
                Err(self.get_worker_err().unwrap_or(InternalError::ChanError))
            }
        }
    }

    pub fn get_worker_err(&mut self) -> Option<InternalError> {
        match self.expect_worker_err_rx {
            Some(ref mut chan) => chan.wait().ok(), // XXX: this will hang if there's no error
            None => None,
        }
    }
}

type ExpectResult = Result<(usize, Vec<u8>), ExpectError>;

impl Drop for Client {
    fn drop(&mut self) {
        // Safe to unwrap, since there's always a shutdown_tx field
        //
        // We also ignore the result of send(), because if it fails, it means the receiver has been
        // dropped already, and so that the EventLoop has been dropped, which is precisely what we're
        // trying to achieve.
        let _ = self.shutdown_tx.take().unwrap().send(());
        // Safe to unwrap, since there's always a thread field
        //
        // Also, if join() fails, it means the child thread panicked, and the error contains the
        // argument given to panic!(), so we just print it.
        if let Err(panic_reason) = self.thread.take().unwrap().join() {
            error!("The EventLoop thread panicked: {:?}", panic_reason);
        }
    }
}

#[derive(Debug)]
pub enum InitError {
    Reactor(io::Error),
    Pty(io::Error),
    SpawnCommand(io::Error),
    SpawnWorker(io::Error),
}

impl Error for InitError {
    fn description(&self) -> &str {
        match *self {
            InitError::Reactor(_) => "failed to start the event loop",
            InitError::Pty(_) => "failed to spawn a pty",
            InitError::SpawnCommand(_) => "failed to spawn the command",
            InitError::SpawnWorker(_) => "failed to spawn an expect thread",
        }
    }

    fn cause(&self) -> Option<&Error> {
        match *self {
            InitError::Reactor(ref e) => Some(e),
            InitError::Pty(ref e) => Some(e),
            InitError::SpawnCommand(ref e) => Some(e),
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

struct EventLoop {
    /// A tokio core handle used to spawn asynchronous tasks.
    handle: Handle,

    /// process stdout async reader
    pty: Pty,

    /// buffer where we store bytes read from stdout
    buffer: Vec<u8>,

    /// Receiver for the input requests coming from the expect handle.
    input_requests_rx: mpsc::UnboundedReceiver<InputRequest>,
    /// FIFO storage for the input requests. Requests are processed one after another, not
    /// concurrently.
    input_requests: VecDeque<InputRequest>,

    /// Receiver for the matching requests coming from the expect handle.
    match_requests_rx: mpsc::UnboundedReceiver<MatchRequest>,

    /// FIFO storage for the matching requests. Requests are processed one after another, not
    /// concurrently.
    match_requests: VecDeque<ActiveMatchRequest>,

    /// Channel to receive shutdown signal from the client
    shutdown_rx: oneshot::Receiver<()>,
}

impl EventLoop {
    fn poll_pending_input<W: Write>(req: &mut InputRequest, input: &mut W) -> Poll<(), io::Error> {
        debug!("processing pending input request");
        let mut size = 0;
        loop {
            match input.write(&req.0[size..]) {
                Ok(i) => {
                    size += i;
                    if size == req.0.len() {
                        return Ok(Async::Ready(()));
                    }
                    // FIXME: do we need to check if we wrote 0 bytes to avoid infinite loops?
                    continue;
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        break;
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        req.0 = req.0.split_off(size);
        Ok(Async::NotReady)
    }

    fn poll_pending_match(
        req: &mut ActiveMatchRequest,
        buffer: &mut Vec<u8>,
        eof: bool,
    ) -> Poll<MatchOutcome, io::Error> {
        debug!("checking pending match request: {:?}", req);

        if let Some((match_index, match_start, match_end)) = Self::try_match(req, buffer) {
            debug!(
                "found match (mach index {}, match range: [{}, {}]",
                match_index, match_start, match_end
            );
            let match_buf = Self::extract_match(buffer, match_start, match_end);
            return Ok(Async::Ready(Ok((match_index, match_buf))));
        }

        if eof {
            // Maybe we were expecting EOF.
            if let Some(i) = req.matches.iter().position(|m| *m == Match::Eof) {
                debug!("found EOF match (mach index {})", i);
                return Ok(Async::Ready(Ok((i, buffer.drain(..).collect()))));
            } else {
                debug!("found unexpected EOF");
                return Ok(Async::Ready(Err(ExpectError::Eof)));
            }
        }

        if let Some(ref mut timeout) = req.timeout {
            debug!("no match found, checking whether this match request timed out already");
            match timeout.poll() {
                Ok(Async::Ready(())) => {
                    if let Some(i) = req.matches.iter().position(|m| *m == Match::Timeout) {
                        debug!("found timeout match (match index {})", i);
                        return Ok(Async::Ready(Ok((i, buffer.drain(..).collect()))));
                    } else {
                        debug!("found unexpected timeout");
                        return Ok(Async::Ready(Err(ExpectError::Timeout)));
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

    fn read_output(&mut self) -> Result<usize, io::Error> {
        debug!("reading from pty");
        let mut buf = [0; 4096];
        let mut size = 0;
        loop {
            match self.pty.read(&mut buf[..]) {
                Ok(i) => {
                    // FIXME: the rust doc say i == 0 *can* mean EOF[0]. This is quite vague, and
                    // `man 2 read` does not help much on linux:
                    //
                    // >  If the file offset is at or past the end of file, no bytes are read, and
                    // > read() returns zero.
                    //
                    // Also, what about other platforms?
                    //
                    // For now, just consider this EOF to avoid looping infinitely
                    //
                    // [0] doc.rust-lang.org/std/io/trait.Read.html/
                    if i == 0 {
                        warn!("0 bytes read from pty");
                        return Err(io::ErrorKind::UnexpectedEof.into());
                    }
                    size += i;
                    self.buffer.extend_from_slice(&buf[..i]);
                    continue;
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        debug!("finished reading {} bytes from pty", size);
                        return Ok(size);
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Read match requests coming from the ClientHandle.
    fn get_match_requests(&mut self) -> Result<(), InternalError> {
        let tokio_handle = self.handle.clone();
        loop {
            match self.match_requests_rx.poll() {
                Ok(Async::Ready(Some(req))) => {
                    self.match_requests.push_back(req.activate(&tokio_handle)?);
                }
                // The channel is closed, which means the ClientHandle has been dropped. This
                // should not happen because the ClientHandle waits for the EventLoop future to
                // finish before being dropped.
                Ok(Async::Ready(None)) => panic!("Cannot receive match requests from ClientHandle"),
                Ok(Async::NotReady) => return Ok(()),
                // There is no documentation suggesting why this might fail, so let's just panic if
                // it happens
                Err(()) => panic!("failed to read from match requests channel"),
            }
        }
    }

    fn get_input_requests(&mut self) {
        debug!("getting input requests from handle");
        loop {
            match self.input_requests_rx.poll() {
                Ok(Async::Ready(Some(req))) => {
                    self.input_requests.push_back(req);
                    debug!("got input request");
                }
                // The channel is closed, which means the ClientHandle has been dropped. This
                // should not happen because the ClientHandle waits for the EventLoop future to
                // finish before being dropped.
                Ok(Async::Ready(None)) => panic!("Cannot receive input requests from ClientHandle"),
                Ok(Async::NotReady) => return,
                // There is no documentation suggesting why this might fail, so let's just panic if
                // it happens
                Err(()) => panic!("failed to read from match requests channel"),
            }
        }
    }

    fn try_match(req: &mut ActiveMatchRequest, buffer: &[u8]) -> Option<(usize, usize, usize)> {
        match req.mode {
            MatchMode::Line => Self::try_match_lines(req, buffer),
            MatchMode::Raw => Self::try_match_raw(req, buffer),
        }
    }

    fn try_match_raw(req: &mut ActiveMatchRequest, buffer: &[u8]) -> Option<(usize, usize, usize)> {
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

    fn try_match_lines(
        req: &mut ActiveMatchRequest,
        buffer: &[u8],
    ) -> Option<(usize, usize, usize)> {
        let lines = get_lines(buffer, LineMode::Full);
        for (i, m) in req.matches.iter().enumerate() {
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

    fn extract_match(buffer: &mut Vec<u8>, start: usize, end: usize) -> Vec<u8> {
        let new_buf = buffer.split_off(end);
        let _matched = buffer.split_off(start);
        // buffer now contains what we want to return
        let ret = buffer.clone();
        *buffer = new_buf;
        ret
    }

    fn process_input(&mut self) {
        debug!("processing input requests");
        let EventLoop {
            ref mut input_requests,
            ref mut pty,
            ..
        } = *self;
        let mut n_requests_processed = 0;
        while let Some(req) = input_requests.get_mut(n_requests_processed) {
            match Self::poll_pending_input(req, pty) {
                Ok(Async::Ready(())) => {
                    debug!("processed input request");
                    n_requests_processed += 1;
                }
                Ok(Async::NotReady) => break,
                Err(e) => panic!("failed to process input request: {}", e),
            }
        }

        assert!(input_requests.len() >= n_requests_processed);
        for _i in 0..n_requests_processed {
            // safe to unwrap as shown by assert above
            let req = input_requests.pop_front().unwrap();
            // FIXME: should we return an error instead?
            if let Err(_) = req.1.send(()) {
                error!("Cannot send input ack to client: the receiver was dropped already");
            }
        }
    }

    fn process_matches(&mut self) -> Result<(), InternalError> {
        let mut eof = false;
        match self.read_output() {
            Ok(i) => debug!("read {} bytes from pty", i),
            Err(e) => {
                // FIXME: not sure if all the errors should be considered EOF.
                eof = true;
            }
        }

        let EventLoop {
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

        assert!(results.len() <= match_requests.len());
        for res in results.drain(..) {
            // unwrapping is safe as shown by previous assert!()
            let req = match_requests.pop_front().unwrap();
            // FIXME: should we return an error instead?
            if let Err(_) = req.response_tx.send(res) {
                error!("Cannot send match outcome to client: the receiver was dropped already");
            }
        }

        Ok(())
    }
}

impl Future for EventLoop {
    type Item = ();
    type Error = InternalError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.get_input_requests();
        if let Err(e) = self.get_match_requests() {
            error!("Failed to process match requests from the client: {}", e);
            return Err(e);
        }

        self.process_input();
        if let Err(e) = self.process_matches() {
            error!("Failed to process match requests from the client: {}", e);
            return Err(e);
        }

        match self.shutdown_rx.poll() {
            Ok(Async::Ready(())) => return Ok(Async::Ready(())),
            Ok(Async::NotReady) => {}
            Err(Canceled) => panic!("Client dropped before the expect worker finished"),
        }
        Ok(Async::NotReady)
    }
}

/// Type used signal errors while the session is running
#[derive(Debug)]
pub enum InternalError {
    SpawnTimeout(io::Error),
    ReadOutput(io::Error),
    NoEventLoop,
    ChanError,
}

impl Error for InternalError {
    fn description(&self) -> &str {
        match *self {
            InternalError::SpawnTimeout(_) => "failed to start a match request timeout future",
            InternalError::ReadOutput(_) => "failed to read output from the pty",
            InternalError::NoEventLoop => "the expect session already errored out",
            InternalError::ChanError => {
                "failed to send or receive data via a channel, probably because one end got dropped"
            }
        }
    }

    fn cause(&self) -> Option<&Error> {
        match *self {
            InternalError::SpawnTimeout(ref e) => Some(e),
            InternalError::ReadOutput(ref e) => Some(e),
            InternalError::NoEventLoop => None,
            InternalError::ChanError => None,
        }
    }
}

impl fmt::Display for InternalError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        if let Some(cause) = self.cause() {
            write!(f, "{}: {}", self.description(), cause)
        } else {
            f.write_str(self.description())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum LineMode {
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
