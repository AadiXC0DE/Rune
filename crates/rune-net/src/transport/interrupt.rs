//! Interruptible socket reads for native provider stream helpers.
//!
//! The reader installs its control on its own thread. The socket transport
//! consults it below TLS, including for pooled connections, without changing
//! the public `Read` body interface or imposing an idle timeout on a stream.

use std::cell::RefCell;
use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ureq::unversioned::transport::{Buffers, ConnectionDetails, Connector, NextTimeout, Transport};

use super::{STREAM_CHUNK_BYTES, STREAM_CHUNKS_AHEAD, STREAM_POLL};

#[derive(Debug, Default)]
struct Control {
    stopped: AtomicBool,
    interruptible: AtomicBool,
}

thread_local! {
    static READER_CONTROL: RefCell<Option<Arc<Control>>> = const { RefCell::new(None) };
}

/// Wraps the socket before TLS, so partial TLS records also remain interruptible.
#[derive(Debug)]
pub(super) struct StreamConnector;

impl<T: Transport> Connector<T> for StreamConnector {
    type Out = StreamTransport<T>;

    fn connect(
        &self,
        details: &ConnectionDetails<'_>,
        chained: Option<T>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        // Preserve DefaultConnector's rejection of HTTPS with a TLS provider
        // absent from this build's feature set, rather than sending plaintext.
        let provider = details.config.tls_config().provider();
        assert!(
            !details.needs_tls()
                || provider == ureq::tls::TlsProvider::Rustls
                || chained.as_ref().is_some_and(Transport::is_tls),
            "uri scheme is https, provider is {provider:?} but feature is not enabled: native-tls"
        );
        Ok(chained.map(StreamTransport))
    }
}

#[derive(Debug)]
pub(super) struct StreamTransport<T>(T);

impl<T: Transport> Transport for StreamTransport<T> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.0.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.0.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let Some(control) = READER_CONTROL.with(|slot| slot.borrow().clone()) else {
            return self.0.await_input(timeout);
        };
        control.interruptible.store(true, Ordering::Release);
        let started = Instant::now();
        loop {
            if control.stopped.load(Ordering::Acquire) {
                return Err(
                    io::Error::new(io::ErrorKind::ConnectionAborted, "stream stopped").into(),
                );
            }
            let remaining = match timeout.after {
                ureq::unversioned::transport::time::Duration::Exact(duration) => {
                    duration.saturating_sub(started.elapsed())
                }
                ureq::unversioned::transport::time::Duration::NotHappening => STREAM_POLL,
            };
            if remaining.is_zero() {
                return Err(ureq::Error::Timeout(timeout.reason));
            }
            match self.0.await_input(NextTimeout {
                after: remaining.min(STREAM_POLL).into(),
                reason: timeout.reason,
            }) {
                // A polling slice expired. Keep waiting under the original
                // deadline, allowing cancellation between socket reads.
                Err(ureq::Error::Timeout(_)) => {}
                result => return result,
            }
        }
    }

    fn is_open(&mut self) -> bool {
        self.0.is_open()
    }

    fn is_tls(&self) -> bool {
        self.0.is_tls()
    }
}

/// Owns the helper and the bounded handover until decoding ends.
#[derive(Debug)]
pub(super) struct StreamReader {
    control: Arc<Control>,
    chunks: Option<Receiver<io::Result<Vec<u8>>>>,
    thread: Option<JoinHandle<()>>,
}

impl StreamReader {
    pub(super) fn spawn(mut body: Box<dyn Read + Send>) -> io::Result<Self> {
        let control = Arc::new(Control::default());
        let reader_control = control.clone();
        let (sender, chunks) = sync_channel(STREAM_CHUNKS_AHEAD);
        let thread = std::thread::Builder::new()
            .name("rune-stream-read".to_owned())
            .spawn(move || {
                READER_CONTROL.with(|slot| *slot.borrow_mut() = Some(reader_control.clone()));
                let mut buffer = vec![0_u8; STREAM_CHUNK_BYTES];
                while !reader_control.stopped.load(Ordering::Acquire) {
                    let chunk = match body.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => Ok(buffer.get(..count).unwrap_or_default().to_vec()),
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                        Err(err) => Err(err),
                    };
                    let failed = chunk.is_err();
                    if sender.send(chunk).is_err() || failed {
                        break;
                    }
                }
            })?;
        Ok(Self {
            control,
            chunks: Some(chunks),
            thread: Some(thread),
        })
    }

    pub(super) fn recv_timeout(
        &self,
        wait: Duration,
    ) -> Result<io::Result<Vec<u8>>, RecvTimeoutError> {
        self.chunks
            .as_ref()
            .ok_or(RecvTimeoutError::Disconnected)?
            .recv_timeout(wait)
    }
}

impl Drop for StreamReader {
    fn drop(&mut self) {
        self.control.stopped.store(true, Ordering::Release);
        // Release a helper blocked on a full handover before joining it.
        drop(self.chunks.take());
        if let Some(thread) = self.thread.take()
            && (self.control.interruptible.load(Ordering::Acquire) || thread.is_finished())
        {
            let _ = thread.join();
        }
        // An arbitrary host-supplied Read has no interruption contract. Keep
        // its existing prompt-return behavior rather than blocking on a join.
    }
}
