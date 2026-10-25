//! Cancellation that reaches into blocked network I/O.
//!
//! ureq is blocking: a read waiting for a silent server only returns when
//! the server answers or a timeout fires. To end a cancelled request at
//! once, every connection the module opens goes through [`CancelConnector`]:
//!
//! * the connector records the socket's [`ShutdownHandle`] (ureq registers
//!   it right after the TCP connect, before the TLS handshake);
//! * the thread running a request installs a [`Token`] with [`scope`]; each
//!   transport read or write on that thread binds the socket it uses to the
//!   token (pooled connections included) and fails once it is cancelled;
//! * [`Token::cancel`] (from any thread) shuts the bound socket down, which
//!   wakes a read or write blocked on it straight away.
//!
//! DNS resolution and the TCP connect itself cannot be interrupted; they are
//! bounded by the connect timeout.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use ureq::unversioned::transport::{Buffers, ConnectionDetails, Connector, DefaultConnector, NextTimeout, ShutdownHandle, Transport};

/// A cancellation flag that can also shut down the socket a request is
/// blocked on.
pub struct Token {
    flag: AtomicBool,
    socket: Mutex<Option<Weak<dyn ShutdownHandle>>>,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Token(cancelled: {})", self.is_cancelled())
    }
}

impl Token {
    pub fn new() -> Arc<Token> {
        Arc::new(Token { flag: AtomicBool::new(false), socket: Mutex::new(None) })
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Cancel: the request's next I/O fails, and I/O blocked right now on
    /// its socket is woken up.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        let socket = self.socket.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(s) = socket.and_then(|w| w.upgrade()) {
            s.shutdown();
        }
    }

    /// Bind the socket the current I/O uses; fail if cancelled.
    fn bind(&self, h: &Weak<dyn ShutdownHandle>) -> Result<(), ureq::Error> {
        {
            let mut g = self.socket.lock().unwrap_or_else(|p| p.into_inner());
            if !g.as_ref().is_some_and(|x| Weak::ptr_eq(x, h)) {
                *g = Some(h.clone());
            }
        }
        // checked after storing: a cancel() racing with this either sees the
        // socket or is seen here
        if self.is_cancelled() {
            if let Some(s) = h.upgrade() {
                s.shutdown();
            }
            return Err(cancelled());
        }
        Ok(())
    }

    fn release(&self) {
        self.socket.lock().unwrap_or_else(|p| p.into_inner()).take();
    }
}

fn cancelled() -> ureq::Error {
    // not `Interrupted`: readers retry those
    ureq::Error::Io(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "cancelled"))
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<Token>>> = const { RefCell::new(None) };
}

fn current() -> Option<Arc<Token>> {
    CURRENT.with(|c| c.borrow().clone())
}

/// Run `f` with `token` as this thread's cancel token: the requests `f`
/// makes on this thread can be interrupted by `token.cancel()`.
pub fn scope<R>(token: &Arc<Token>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Arc<Token>>, Arc<Token>);
    impl Drop for Restore {
        fn drop(&mut self) {
            // a pooled connection must not be shut down by a later cancel
            self.1.release();
            let prev = self.0.take();
            CURRENT.with(|c| *c.borrow_mut() = prev);
        }
    }
    let prev = CURRENT.with(|c| c.borrow_mut().replace(token.clone()));
    let _restore = Restore(prev, token.clone());
    f()
}

/// The default connector chain (TCP, TLS with rustls), with every transport
/// wrapped so that the current [`Token`] can interrupt it.
#[derive(Debug, Default)]
pub struct CancelConnector {
    inner: DefaultConnector,
}

impl Connector for CancelConnector {
    type Out = CancelTransport;

    fn connect(&self, details: &ConnectionDetails, chained: Option<()>) -> Result<Option<CancelTransport>, ureq::Error> {
        let token = current();
        if token.as_ref().is_some_and(|t| t.is_cancelled()) {
            return Err(cancelled());
        }
        let seen: Arc<Mutex<Option<Weak<dyn ShutdownHandle>>>> = Arc::new(Mutex::new(None));
        let (seen2, token2, forward) = (seen.clone(), token.clone(), details.register_shutdown.clone());
        let register = Arc::new(move |w: Weak<dyn ShutdownHandle>| -> Result<(), ureq::Error> {
            forward(w.clone())?;
            *seen2.lock().unwrap_or_else(|p| p.into_inner()) = Some(w.clone());
            // bind at once: the TLS handshake that follows is interruptible too
            match &token2 {
                Some(t) => t.bind(&w),
                None => Ok(()),
            }
        });
        let d = ConnectionDetails {
            uri: details.uri,
            addrs: details.addrs.clone(),
            config: details.config,
            request_level: details.request_level,
            resolver: details.resolver,
            now: details.now,
            timeout: details.timeout,
            current_time: details.current_time.clone(),
            run_connector: details.run_connector.clone(),
            register_shutdown: register,
        };
        let out = self.inner.connect(&d, chained)?;
        let socket = seen.lock().unwrap_or_else(|p| p.into_inner()).take();
        Ok(out.map(|inner| CancelTransport { inner, socket }))
    }
}

/// A transport that checks the current [`Token`] before each read and write.
#[derive(Debug)]
pub struct CancelTransport {
    inner: Box<dyn Transport>,
    socket: Option<Weak<dyn ShutdownHandle>>,
}

impl CancelTransport {
    fn check(&self) -> Result<(), ureq::Error> {
        CURRENT.with(|c| match (c.borrow().as_ref(), &self.socket) {
            (Some(t), Some(s)) => t.bind(s),
            (Some(t), None) if t.is_cancelled() => Err(cancelled()),
            _ => Ok(()),
        })
    }
}

impl Transport for CancelTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.check()?;
        self.inner.transmit_output(amount, timeout)
    }
    fn maybe_await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.check()?;
        self.inner.maybe_await_input(timeout)
    }
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.check()?;
        self.inner.await_input(timeout)
    }
    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }
    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}
