//! Resume interrupted socket reads without replaying an HTTP request.

use std::io::ErrorKind;
use std::time::Instant;

use ureq::Error;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector as UreqConnector, DefaultConnector, NextTimeout,
    Transport as UreqTransport,
};

#[derive(Debug, Default)]
pub(super) struct Connector(DefaultConnector);

impl UreqConnector for Connector {
    type Out = Transport;

    fn connect(
        &self,
        details: &ConnectionDetails<'_>,
        chained: Option<()>,
    ) -> Result<Option<Self::Out>, Error> {
        self.0
            .connect(details, chained)
            .map(|transport| transport.map(Transport))
    }
}

#[derive(Debug)]
pub(super) struct Transport(Box<dyn UreqTransport>);

impl UreqTransport for Transport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.0.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        self.0.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        // ureq's TCP transport forwards EINTR from read, unlike Read::read_exact.
        // The read made no progress, so resume it on the same connection and
        // buffers. Reissuing the request could execute a tool twice.
        resume_read(timeout, |next| self.0.await_input(next))
    }

    fn is_open(&mut self) -> bool {
        self.0.is_open()
    }

    fn is_tls(&self) -> bool {
        self.0.is_tls()
    }
}

fn resume_read<T>(
    timeout: NextTimeout,
    mut read: impl FnMut(NextTimeout) -> Result<T, Error>,
) -> Result<T, Error> {
    let started = Instant::now();
    let mut next = timeout;
    loop {
        match read(next) {
            Err(Error::Io(error)) if error.kind() == ErrorKind::Interrupted => {
                if !timeout.after.is_not_happening() {
                    let remaining = timeout.after.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        return Err(Error::Timeout(timeout.reason));
                    }
                    next.after = remaining.into();
                }
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn timeout(after: Duration) -> NextTimeout {
        NextTimeout {
            after: after.into(),
            reason: ureq::Timeout::Global,
        }
    }

    #[test]
    fn interrupted_reads_resume_with_the_remaining_budget() {
        let original = timeout(Duration::from_secs(5));
        let mut calls = 0;
        let result = resume_read(original, |next| {
            assert_eq!(next.reason, original.reason);
            assert!(next.after <= original.after);
            calls += 1;
            if calls < 3 {
                Err(std::io::Error::from(ErrorKind::Interrupted).into())
            } else {
                Ok("the same response")
            }
        });
        assert_eq!(result.expect("read resumed"), "the same response");
        assert_eq!(calls, 3);
    }

    #[test]
    fn interruption_does_not_restart_an_expired_budget() {
        let mut calls = 0;
        let result: Result<(), Error> = resume_read(timeout(Duration::ZERO), |_| {
            calls += 1;
            Err(std::io::Error::from(ErrorKind::Interrupted).into())
        });
        assert!(matches!(result, Err(Error::Timeout(ureq::Timeout::Global))));
        assert_eq!(calls, 1);
    }

    #[test]
    fn other_read_errors_are_returned_immediately() {
        let mut calls = 0;
        let result: Result<(), Error> = resume_read(timeout(Duration::from_secs(5)), |_| {
            calls += 1;
            Err(std::io::Error::from(ErrorKind::ConnectionReset).into())
        });
        assert!(
            matches!(result, Err(Error::Io(error)) if error.kind() == ErrorKind::ConnectionReset)
        );
        assert_eq!(calls, 1);
    }
}
