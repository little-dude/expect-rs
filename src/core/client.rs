use futures::Future;
use futures::sync::{mpsc, oneshot};

use super::errors::{ExpectError, SendError};
use super::match_request::{InputRequest, MatchRequest, MatchRequestBuilder};

#[derive(Debug)]
pub struct Client {
    /// Channel to send match requests to the expect session
    pub(crate) match_requests_tx: mpsc::UnboundedSender<MatchRequest>,
    /// Channel to send input requests to the expect session
    pub(crate) input_requests_tx: mpsc::UnboundedSender<InputRequest>,
    /// Channel to send a shutdown signal to the expect session
    pub(crate) shutdown_tx: Option<oneshot::Sender<()>>,
    /// Channel to receive the exit status from the expect session.
    pub(crate) exit_status_rx: Option<oneshot::Receiver<()>>,
}

impl Client {
    pub fn send(&mut self, bytes: Vec<u8>) -> Result<(), SendError> {
        let (response_tx, response_rx) = oneshot::channel::<()>();
        match self.input_requests_tx
            .unbounded_send(InputRequest(bytes, response_tx))
        {
            Ok(()) => response_rx.wait().or_else(|_| {
                error!("failed to receive response from the expect session");
                Err(SendError::ChanError)
            }),
            Err(_) => {
                error!("failed to send input request to the expect session");
                Err(SendError::ChanError)
            }
        }
    }

    pub fn expect(&mut self, builder: MatchRequestBuilder) -> ExpectResult {
        let (request, response_rx) = builder.build();
        match self.match_requests_tx.unbounded_send(request) {
            Ok(()) => response_rx.wait().or_else(|_| {
                error!("failed to receive response from the expect session");
                Err(ExpectError::ChanError)
            })?,
            Err(_) => {
                error!("failed to send match request to the expect session");
                Err(ExpectError::ChanError)
            }
        }
    }

    pub fn close(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            if shutdown_tx.send(()).is_ok() {
                return self.wait_for_exit_status();
            }
        }
    }

    fn wait_for_exit_status(&mut self) {
        self.exit_status_rx
            .take()
            // Safe to unwrap, because exit_status_rx is consumed with shutdown_tx. If shutdown_tx
            // has not yet be consumed already, then exit_status_rx has not been consumed either.
            .expect("internal error")
            .wait()
            // cannot fail because the session (and so the sender) is not dropped before the exit
            // status has been sent.
            .expect("failed to receive exit status from session");
    }
}

type ExpectResult = Result<(usize, Vec<u8>), ExpectError>;

impl Drop for Client {
    fn drop(&mut self) {
        self.close();
    }
}
