//! Provides a scoped interface to signal handlers that are only active for the lifetime of the
//! a given closure

use std::cell::Cell;
use std::marker::PhantomData;
use std::mem;
use std::sync::atomic::{compiler_fence, Ordering};

use arr_macro::arr;

pub use libc::siginfo_t as SigInfo;
use libc::{c_int, c_void};
use nix::sys::signal;
pub use nix::sys::signal::{SaFlags, SigSet, Signal, SigAction, SigHandler};
pub use nix::Error;

thread_local!(static SIGNAL_HANDLERS: [Cell<Option<&'static dyn Fn(u8, &SigInfo)>>; 64] = arr![Cell::from(None); 64]);

extern "C" fn c_handler(signo: c_int, info: *mut SigInfo, _: *mut c_void) {
    // I hope the `with` method is async-signal-safe
    SIGNAL_HANDLERS.with(|handlers| {
        if let Some(h) = handlers[signo as usize].get() {
            if let Some(info) = unsafe { info.as_ref() } {
                h(signo as u8, info)
            }
        }
    });
}

// this struct is necessary in the event of an unwinding panic to ensure the handler fn is not left
// in thread local storage longer than its lifetime
struct HandlerGuard<'f, F: Fn(u8, &SigInfo) + 'f> {
    signal: Signal,
    handler: PhantomData<&'f F>,
    old: Option<&'static dyn Fn(u8, &SigInfo)>,
}

impl<'f, F: Fn(u8, &SigInfo) + 'f> HandlerGuard<'f, F> {
    fn install(signal: Signal, handler: &'f F) -> Self {
        let old = SIGNAL_HANDLERS.with(|handlers| {
            let fn_object = handler as &dyn Fn(u8, &SigInfo);
            // safe because we ensure the value is only stored in the variable for the lifetime of this
            // object, and we do not leak the reference anywhere else
            let static_fn = unsafe {
                // type/lifetime annotations save lives when using mem::transmute
                mem::transmute::<&'f dyn Fn(u8, &SigInfo), &'static dyn Fn(u8, &SigInfo)>(fn_object)
            };
            handlers[signal as usize].replace(Some(static_fn))
        });
        Self {
            signal,
            handler: Default::default(),
            old,
        }
    }
}

impl<'f, F: Fn(u8, &SigInfo) + 'f> Drop for HandlerGuard<'f, F> {
    // drop handlers get run on unwind YEET
    fn drop(&mut self) {
        // pull the signal handler we installed back out
        let _mine = SIGNAL_HANDLERS
            .with(|handlers| handlers[self.signal as usize].replace(self.old.take()));
    }
}

/// Install a signal handler only valid for a given scope
pub struct SignalScope<F> {
    handler: F,
    signal: Signal,
    flags: SaFlags,
    set: SigSet,
}

impl<Handler: Fn(u8, &SigInfo)> SignalScope<Handler> {
    /// Create an object representing the provided signal handler.
    /// The handler is only called in the thread that run() is called from.
    /// If another thread receives the same signal, the signal will be ignored unless the thread
    /// is using its own SignalScope for the same signal
    ///
    /// `set` defines what signals are blocked during the execution of the signal handler itself
    ///
    /// See `sigaction(3P)` for more info
    /// # Safety
    /// This is an unsafe operation because the passed `handler` must only call async-signal-safe
    /// functions and we cannot verify this
    pub unsafe fn new(signal: Signal, flags: SaFlags, set: SigSet, handler: Handler) -> Self {
        Self {
            handler,
            signal,
            flags,
            set,
        }
    }

    /// Run the given closure with this signal handler installed
    pub fn run<T, F: FnOnce() -> T>(self, f: F) -> Result<T, Error> {
        let action = SigHandler::SigAction(c_handler);
        let sa = SigAction::new(action, self.flags, self.set);

        // load our handler
        let guard = HandlerGuard::install(self.signal, &self.handler);
        let _old_handler = unsafe { signal::sigaction(self.signal, &sa)? };

        compiler_fence(Ordering::SeqCst);
        let ret = Ok(f());
        compiler_fence(Ordering::SeqCst);

        // uninstall the handler fn from TLS
        drop(guard);

        // we no longer reinstall the old handler

        ret
    }
}
