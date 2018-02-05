use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::process::{Child, Command};

use futures::{Async, Canceled, Future, Poll, Stream};
use futures::sync::{mpsc, oneshot};
use tokio_core::reactor::Handle;

use super::client::Client;
use super::errors::{ExpectError, SpawnError};
use super::match_request::{InputRequest, MatchOutcome, MatchRequest};
use super::pty::Pty;

pub struct Session {
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
    match_requests: VecDeque<MatchRequest>,

    /// Channel to receive shutdown signal from the client
    shutdown_rx: oneshot::Receiver<()>,

    exit_status_tx: Option<oneshot::Sender<()>>,

    handle: Handle,
    child: Child,
}

impl Session {
    fn write_input<W: Write>(req: &mut InputRequest, input: &mut W) -> Poll<(), io::Error> {
        debug!("processing pending input request");
        let mut size = 0;
        loop {
            match input.write(&req.0[size..]) {
                Ok(i) => {
                    size += i;
                    if size == req.0.len() {
                        // The whole buffer has been written, we're done.
                        return Ok(Async::Ready(()));
                    }
                    // FIXME: do we need to check if we wrote 0 bytes to avoid infinite loops?
                    // I'm not sure this can really happen though.
                    continue;
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        // The file descriptor is opened with O_NONBLOCK, so on EWOULDBLOCK just
                        // stop trying to write, the file descriptor is not ready for that at the
                        // moment.
                        break;
                    } else {
                        // Propagate any other error.
                        return Err(e);
                    }
                }
            }
        }
        req.0 = req.0.split_off(size);
        Ok(Async::NotReady)
    }

    /// Try to find a match for the given request
    fn find_match(req: &mut MatchRequest, buffer: &mut Vec<u8>, eof: bool) -> Option<MatchOutcome> {
        // Check whether a match can be found in the buffer (case 1)
        if let Some(m) = req.match_buffer(buffer) {
            return Some(Ok(m));
        }

        if eof {
            // Check whether we were expecting this EOF (case 2). If so, return successfully,
            // otherwise, error out.
            if let Some(i) = req.get_eof_match_index() {
                debug!("found EOF match (mach index {})", i);
                return Some(Ok((i, buffer.drain(..).collect())));
            } else {
                debug!("found unexpected EOF");
                return Some(Err(ExpectError::Eof));
            }
        }

        debug!("no match found, checking whether this match request timed out already");
        match req.timed_out() {
            Ok(true) => {
                if let Some(i) = req.get_timeout_match_index() {
                    debug!("found timeout match (match index {})", i);
                    Some(Ok((i, buffer.drain(..).collect())))
                } else {
                    debug!("found unexpected timeout");
                    Some(Err(ExpectError::Timeout))
                }
            }
            Ok(false) => {
                debug!("the match request did not time out yet");
                None
            }
            Err(e) => {
                error!("Failed to check whether the request timed out: {}", e);
                Some(Err(ExpectError::Io(e)))
            }
        }
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
                    // The file descriptor is non-blocking, so it is expected to get this error
                    // once there are no more bytes left to read.
                    if e.kind() == io::ErrorKind::WouldBlock {
                        debug!("finished reading {} bytes from pty", size);
                        return Ok(size);
                    }
                    // Propagate any other error.
                    return Err(e);
                }
            }
        }
    }

    fn handle_new_request(&mut self, mut request: MatchRequest) {
        // Try to start the timeout on the new request
        if let Err(e) = request.activate(&self.handle.clone()) {
            error!("Failed to activate match request");
            request.send_outcome(Err(ExpectError::Io(e)));
        } else {
            // Timeout started. Keep track of the request
            self.match_requests.push_back(request);
        }
    }

    /// Read match requests coming from the ClientHandle.
    fn get_match_requests(&mut self) {
        loop {
            match self.match_requests_rx.poll() {
                Ok(Async::Ready(Some(req))) => self.handle_new_request(req),
                // The channel is closed, which means the client has been dropped. This should not
                // happen because the client waits for the Session future to finish before being
                // dropped.
                Ok(Async::Ready(None)) => panic!("Cannot receive match requests from ClientHandle"),
                // No more requests
                Ok(Async::NotReady) => return,
                // There is no documentation suggesting why this might fail, so let's just panic if
                // it happens. Normally, when all the senders have been dropped, we can just read
                // all pending values until poll() returns AsyncReady(None).
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
                // The channel is closed, which means the client has been dropped. This should not
                // happen because the client waits for the Session future to finish before being
                // dropped.
                Ok(Async::Ready(None)) => panic!("Cannot receive input requests from ClientHandle"),
                // No more requests
                Ok(Async::NotReady) => return,
                // There is no documentation suggesting why this might fail, so let's just panic if
                // it happens. Normally, when all the senders have been dropped, we can just read
                // all pending values until poll() returns AsyncReady(None).
                Err(()) => panic!("failed to read from match requests channel"),
            }
        }
    }

    fn process_input_requests(&mut self) {
        debug!("processing input requests");
        let Session {
            ref mut input_requests,
            ref mut pty,
            ..
        } = *self;
        let mut n_requests_processed = 0;

        while let Some(req) = input_requests.get_mut(n_requests_processed) {
            match Self::write_input(req, pty) {
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

    fn process_match_requests(&mut self) {
        let eof = match self.read_output() {
            Ok(i) => {
                debug!("read {} bytes from pty", i);
                false
            }
            Err(_) => true, // FIXME: not sure if all the errors should be considered EOF.
        };

        let Session {
            ref mut match_requests,
            ref mut buffer,
            ..
        } = *self;

        // The reason for the two loops is subtile here. In theory, we could check check the
        // requests until we find one for which there is not match, and ignore the remaining
        // requests. BUT, all these requests are running a timeout task, that MUST be polled,
        // otherwise they may finish and not wake up the event loop again, thus hanging the whole
        // thread.

        let mut unfinished_requests: VecDeque<MatchRequest> = VecDeque::new();

        // extra scope to workaround borrock limitations (should be removable when non lexical
        // lifetimes are stabilized)
        {
            let mut drain = match_requests.drain(..);

            for mut req in &mut drain {
                match Self::find_match(&mut req, buffer, eof) {
                    Some(outcome) => req.send_outcome(outcome),
                    None => {
                        unfinished_requests.push_back(req);
                        break;
                    }
                }
            }

            // For the remaining requests, only poll the timeouts. We don't even care if they timed
            // out, all that matters is to poll the timeout tasks.
            for mut req in drain {
                let _ = req.timed_out();
                unfinished_requests.push_back(req);
            }
        }

        *match_requests = unfinished_requests;
    }

    pub fn spawn(cmd: Command, handle: Handle) -> Result<Client, SpawnError> {
        debug!("spawning new command {:?}", cmd);

        // Chan used by the client to send input requests to the session when client.send() is
        // called.
        let (input_tx, input_rx) = mpsc::unbounded::<InputRequest>();

        // Chan used by the client to send match requests to the session when client.match() is
        // called.
        let (match_tx, match_rx) = mpsc::unbounded::<MatchRequest>();

        // Chan used by the client to ask the session to terminate. This is used when the client
        // gets dropped.
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        // Chan used by the session to send its exit status to the client when shutting down
        // gets dropped.
        let (exit_status_tx, exit_status_rx) = oneshot::channel::<()>();

        // Create a new PTY and spawn the command, attaching its stdin, stdout and stderr to the
        // slave PTY. The master PTY's file descriptor get registered on the event loop so that we
        // can perform async reads and writes.
        let mut pty = Pty::new::<::std::fs::File>(None, &handle).map_err(|e| SpawnError::Pty(e))?;
        let child = pty.spawn(cmd).map_err(|e| SpawnError::SpawnCommand(e))?;

        let session = Session {
            pty,
            buffer: Vec::new(),
            input_requests_rx: input_rx,
            match_requests_rx: match_rx,
            input_requests: VecDeque::new(),
            match_requests: VecDeque::new(),
            shutdown_rx,
            child,
            handle: handle.clone(),
            exit_status_tx: Some(exit_status_tx),
        };
        handle.spawn(session);

        Ok(Client {
            match_requests_tx: match_tx,
            input_requests_tx: input_tx,
            shutdown_tx: Some(shutdown_tx),
            exit_status_rx: Some(exit_status_rx),
        })
    }

    /// Send a successful exit status to the client
    fn exit(&mut self) {
        if let Some(tx) = self.exit_status_tx.take() {
            // Cannot fail since the client cannot be dropped before receiving the exit status.
            tx.send(()).expect("Could not send exit status to client");
        }
    }

    /// Check whether the client asked the session to terminate
    fn should_exit(&mut self) -> bool {
        match self.shutdown_rx.poll() {
            Ok(Async::Ready(())) => true,
            Ok(Async::NotReady) => false,
            Err(Canceled) => panic!("Client dropped before the expect worker finished"),
        }
    }
}

impl Future for Session {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.get_input_requests();
        self.process_input_requests();
        self.get_match_requests();
        self.process_match_requests();
        if self.should_exit() {
            self.exit();
            Ok(Async::Ready(()))
        } else {
            Ok(Async::NotReady)
        }
    }
}
