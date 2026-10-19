use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::api::Error;

#[derive(Clone, Debug)]
pub struct RequestContext {
    pub deadline: Instant,
    pub cancellation: CancellationToken,
}

impl RequestContext {
    pub fn new(timeout: Duration) -> Self {
        Self::with_token(timeout, CancellationToken::new())
    }

    pub fn with_token(timeout: Duration, token: CancellationToken) -> Self {
        let now = Instant::now();
        Self {
            deadline: now.checked_add(timeout).unwrap_or(now),
            cancellation: token,
        }
    }

    pub fn remaining(&self) -> Result<Duration, Error> {
        if self.cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(Error::timeout)
    }

    pub fn check(&self) -> Result<(), Error> {
        self.remaining().map(|_| ())
    }

    pub fn child(&self) -> Self {
        Self {
            deadline: self.deadline,
            cancellation: self.cancellation.child_token(),
        }
    }
}
