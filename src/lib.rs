extern crate futures;
extern crate libc;
#[macro_use]
extern crate log;
extern crate mio;
extern crate regex;
extern crate tokio_core;
extern crate tokio_io;
// extern crate pty;
extern crate tty;

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

mod pty;
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

pub struct Session {
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

    drop_rx: oneshot::Receiver<()>,
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
    /// Start the timeout future for the match request, effectively "activating" the match request.
    fn activate(self, handle: &Handle) -> Result<ActiveMatchRequest, io::Error> {
        let timeout = if let Some(duration) = self.timeout {
            Some(Timeout::new(duration, handle)?)
        } else {
            None
        };

        Ok(ActiveMatchRequest {
            matches: self.matches,
            response_tx: self.response_tx,
            timeout: timeout,
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

pub struct Client {
    match_requests_tx: mpsc::UnboundedSender<MatchRequest>,
    input_requests_tx: mpsc::UnboundedSender<InputRequest>,
    thread: Option<thread::JoinHandle<()>>,
    drop_tx: Option<oneshot::Sender<()>>,
}

impl Client {
    pub fn send(&self, bytes: Vec<u8>) -> () {
        let handle = self.clone();
        let (response_tx, response_rx) = oneshot::channel::<()>();
        // Sending fails if the receiver has been dropped which should not happen as long as there
        // is a client. So it is safe to unwrap.
        handle
            .input_requests_tx
            .unbounded_send(InputRequest(bytes, response_tx))
            .unwrap();
        // There is no documentation suggesting receiving can fail so we assume it cannot.
        response_rx.wait().unwrap();
    }

    pub fn expect(&mut self, matches: Vec<Match>, timeout: Option<Duration>) -> ExpectResult {
        let (response_tx, response_rx) = oneshot::channel::<MatchOutcome>();
        let request = MatchRequest {
            matches,
            response_tx,
            timeout,
        };
        // Sending fails if the receiver has been dropped which should not happen as long as there
        // is a client. So it is safe to unwrap.
        self.match_requests_tx.unbounded_send(request).unwrap();
        // There is no documentation suggesting receiving can fail so we assume it cannot.
        response_rx.wait().unwrap()
    }
}

type ExpectResult = Result<(usize, std::vec::Vec<u8>), ExpectError>;

// FIXME: This Drop implementation assume we have only one client. Ultimately, we want Client to be
// Clone-able.
impl Drop for Client {
    fn drop(&mut self) {
        // Safe to unwrap, since there's always a drop_tx field
        //
        // We also ignore the result of send(), because if it fails, it means the receiver has been
        // dropped already, and so that the Session has been dropped, which is precisely what we're
        // trying to achieve.
        let _ = self.drop_tx.take().unwrap().send(());
        // Safe to unwrap, since there's always a thread field
        //
        // Also, if join() fails, it means the child thread panicked, and the error contains the
        // argument given to panic!(), so we just print it.
        if let Err(panic_reason) = self.thread.take().unwrap().join() {
            error!("The Session thread panicked: {:?}", panic_reason);
        }
    }
}

#[derive(Debug)]
pub enum InitError {
    Reactor(io::Error),
    Pty(io::Error),
    Spawn(io::Error),
}

impl Error for InitError {
    fn description(&self) -> &str {
        use InitError::*;
        match *self {
            Reactor(_) => "failed to start the event loop",
            Pty(_) => "failed to spawn a pty",
            Spawn(_) => "failed to spawn the command",
        }
    }

    fn cause(&self) -> Option<&Error> {
        use InitError::*;
        match *self {
            Reactor(ref e) => Some(e),
            Pty(ref e) => Some(e),
            Spawn(ref e) => Some(e),
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

impl Session {
    pub fn spawn(cmd: Command) -> Result<Client, InitError> {
        debug!("spawning new command {:?}", cmd);
        let (input_tx, input_rx) = mpsc::unbounded::<InputRequest>();
        let (match_tx, match_rx) = mpsc::unbounded::<MatchRequest>();
        let (drop_tx, drop_rx) = oneshot::channel::<()>();

        let (err_tx, err_rx) = oneshot::channel::<Result<(), InitError>>();

        // spawn the core future in a separate thread.
        //
        // it's bad practice to spawn our own core but otherwise, it's complicated to provide a
        // synchronous client, since calling `wait()` blocks the current thread, preventing the
        // event loop from making progress... But running the event loop in a separate thread, we
        // can call `wait()` in the client.
        //
        // FIXME: the error handling is pretty tedious here... not sure whether we can do anything
        // about that since it all happens in a separate thread.
        let thread = thread::Builder::new()
            .name("expect-event-loop".into())
            .spawn(move || {
                let mut core = match Core::new() {
                    Ok(core) => core,
                    Err(e) => {
                        // this cannot fail as long as the receiver has not been dropped
                        err_tx.send(Err(InitError::Reactor(e))).unwrap();
                        return;
                    }
                };

                let mut pty = match Pty::new::<::std::fs::File>(None, &core.handle()) {
                    Ok(pty) => pty,
                    Err(e) => {
                        // this cannot fail as long as the receiver has not been dropped
                        err_tx.send(Err(InitError::Pty(e))).unwrap();
                        return;
                    }
                };

                // FIXME: I guess we should do something with the child?
                let _child = match pty.spawn(cmd) {
                    Ok(child) => child,
                    Err(e) => {
                        // this cannot fail as long as the receiver has not been dropped
                        err_tx.send(Err(InitError::Spawn(e))).unwrap();
                        return;
                    }
                };

                // this cannot fail as long as the receiver has not been dropped
                err_tx.send(Ok(())).unwrap();

                let session = Session {
                    pty: pty,
                    handle: core.handle(),
                    buffer: Vec::new(),
                    input_requests_rx: input_rx,
                    match_requests_rx: match_rx,
                    input_requests: VecDeque::new(),
                    match_requests: VecDeque::new(),
                    drop_rx: drop_rx,
                };

                core.run(session).unwrap();
            })
            .unwrap();

        // wait() cannot fail on the receiver
        err_rx.wait().unwrap()?;

        Ok(Client {
            match_requests_tx: match_tx.clone(),
            input_requests_tx: input_tx.clone(),
            thread: Some(thread),
            drop_tx: Some(drop_tx),
        })
    }

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
                return Ok(Async::Ready(Ok((i, buffer.clone()))));
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
                        return Ok(Async::Ready(Ok((i, buffer.clone()))));
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
                    if i == 0 {
                        warn!("met EOF while reading from pty");
                        return Err(io::ErrorKind::UnexpectedEof.into());
                    }
                    size += i;
                    self.buffer.extend_from_slice(&buf[..i]);
                    continue;
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        debug!("done reading from pty");
                        debug!("buffer so far: {}", String::from_utf8_lossy(&self.buffer));
                        return Ok(size);
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Read match requests coming from the ClientHandle.
    fn get_match_requests(&mut self) -> Result<(), ()> {
        let tokio_handle = self.handle.clone();
        loop {
            match self.match_requests_rx.poll() {
                Ok(Async::Ready(Some(req))) => {
                    // FIXME: error handling
                    self.match_requests
                        .push_back(req.activate(&tokio_handle).unwrap())
                }
                // The channel is closed, which means the ClientHandle has been dropped. This
                // should not happen because the ClientHandle waits for the Session future to
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
                // should not happen because the ClientHandle waits for the Session future to
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
        let new_buf = buffer.split_off(end);
        let _matched = buffer.split_off(start);
        // buffer now contains what we want to return
        let ret = buffer.clone();
        *buffer = new_buf;
        ret
    }

    fn process_input(&mut self) {
        debug!("processing input requests");
        let Session {
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
        for _i in 0..n_requests_processed {
            let req = input_requests.pop_front().unwrap();
            req.1.send(()).unwrap();
        }
    }

    fn process_matches(&mut self) -> Result<(), io::Error> {
        let mut eof = false;
        match self.read_output() {
            Ok(i) => {
                debug!("read {} bytes from pty", i);
            }
            Err(e) => {
                if e.kind() == io::ErrorKind::UnexpectedEof {
                    debug!("EOF in pty");
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
        self.get_input_requests();
        if let Err(_e) = self.get_match_requests() {
            return Err(());
        }
        self.process_input();
        if let Err(_e) = self.process_matches() {
            return Err(());
        }
        match self.drop_rx.poll() {
            Ok(Async::Ready(())) => return Ok(Async::Ready(())),
            Ok(Async::NotReady) => {}
            Err(Canceled) => return Err(()),
        }
        Ok(Async::NotReady)
    }
}
