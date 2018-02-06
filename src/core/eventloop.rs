use std::io;
use std::process::Command;
use std::thread::{self, JoinHandle};

use futures::{Async, Canceled, Future, Poll, Stream};
use futures::sync::{mpsc, oneshot};
use tokio_core::reactor::{Core, Handle};

use super::client::Client;
use super::errors::{InitError, SpawnError};
use super::session::Session;

pub struct ExpectHandle {
    spawn_tx: mpsc::UnboundedSender<SpawnRequest>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    exit_rx: Option<oneshot::Receiver<()>>,
    evl_thread: Option<JoinHandle<()>>,
}

impl ExpectHandle {
    pub fn new() -> Result<Self, InitError> {
        let (init_res_tx, init_res_rx) = oneshot::channel::<Result<(), io::Error>>();
        let (spawn_tx, spawn_rx) = mpsc::unbounded::<SpawnRequest>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (exit_tx, exit_rx) = oneshot::channel::<()>();

        let thread = thread::Builder::new()
            .name("expect-worker".into())
            .spawn(move || {
                let mut core = match Core::new() {
                    Ok(core) => {
                        // this cannot fail as long as the receiver has not been dropped
                        init_res_tx.send(Ok(())).unwrap();
                        core
                    }
                    Err(e) => {
                        // this cannot fail as long as the receiver has not been dropped
                        init_res_tx.send(Err(e)).unwrap();
                        return;
                    }
                };

                let eventloop = EventLoop {
                    handle: core.handle(),
                    spawn_rx,
                    shutdown_rx,
                };

                if let Err(()) = core.run(eventloop) {
                    error!("expect worker terminated with an error");
                }

                exit_tx
                    .send(())
                    .expect("expect worker: failed to send error to main thread");
            })
            .map_err(InitError::SpawnWorker)?;

        init_res_rx.wait().unwrap().map_err(InitError::Reactor)?;

        Ok(ExpectHandle {
            spawn_tx,
            shutdown_tx: Some(shutdown_tx),
            exit_rx: Some(exit_rx),
            evl_thread: Some(thread),
        })
    }

    pub fn spawn(&mut self, cmd: Command) -> Result<Client, SpawnError> {
        let (client_tx, client_rx) = oneshot::channel::<Result<Client, SpawnError>>();
        let request = (cmd, client_tx);
        self.spawn_tx
            .unbounded_send(request)
            .map_err(|_| SpawnError::NoEventLoop)?;
        client_rx
            .wait()
            .expect("failed to receive client from event loop")
    }
}

impl Drop for ExpectHandle {
    fn drop(&mut self) {
        // we unwrap when taking shutdown_rx and exit_rx because this is the only place they are
        // used. Ideally, we shouldn't even have to make these Options, but since drop takes &mut
        // self instead of self, and since the two channels are consumed, we have to use this
        // trick.
        if self.shutdown_tx.take().unwrap().send(()).is_ok() {
            // cannot fail because the session (and so the sender) is not dropped before
            // the exit status has been sent.
            self.exit_rx
                .take()
                .unwrap()
                .wait()
                .expect("failed to receive exit status from eventloop");
            let _ = self.evl_thread.take().unwrap().join();
        }
    }
}

pub struct EventLoop {
    handle: Handle,
    spawn_rx: mpsc::UnboundedReceiver<SpawnRequest>,
    shutdown_rx: oneshot::Receiver<()>,
}

pub type SpawnRequest = (Command, oneshot::Sender<Result<Client, SpawnError>>);

impl EventLoop {
    fn process_spawn_requests(&mut self) {
        debug!("processing spawn requests");
        loop {
            // No idea why this would fail. Let's panic if that happens.
            match self.spawn_rx
                .poll()
                .expect("failed to read spawn requests from channel")
            {
                Async::Ready(Some(request)) => self.spawn(request),
                // The channel is closed, which means the expect handle has been dropped. This
                // should not happen because the handle waits for the eventloop future to finish
                // before being dropped.
                Async::Ready(None) => panic!("Cannot receive input requests from client"),
                // no more request to read
                Async::NotReady => return,
            }
        }
    }

    fn spawn(&mut self, request: SpawnRequest) {
        let (cmd, client_tx) = request;
        if client_tx.send(Session::spawn(cmd, &self.handle)).is_err() {
            // We deliberately ignore the result of the send(): if the receiver was
            // dropped, we don't care if sending failed anyway.
            error!("Failed to send the spawn result back to the expect handle (receiver dropped)");
        }
    }
}

impl Future for EventLoop {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.process_spawn_requests();
        match self.shutdown_rx.poll() {
            Ok(Async::Ready(())) => {
                debug!("Exiting event loop");
                Ok(Async::Ready(()))
            }
            Ok(Async::NotReady) => Ok(Async::NotReady),
            // In theory, we could avoid panicking here but I'd rather having the handle dropped
            // before the session is something I'd like to avoid, so we're a bit extreme here.
            Err(Canceled) => panic!("handle dropped before the event loop worker finished"),
        }
    }
}
