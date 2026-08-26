//! Keeping a panic's stack, which `catch_unwind` throws away.
//!
//! Lives here rather than in `rainier-debug`, where it is used, because the
//! *kernel* is what catches the panic — and `rainier-debug` depends on
//! `rainier-server`, so the kernel cannot depend back on it. `rainier-support`
//! is the crate everything may depend on, which makes it the only place a
//! primitive shared by those two can sit.
//!
//! The kernel catches a panic in a handler and turns it into a 500. What it
//! gets from [`std::panic::catch_unwind`] is the payload — the `&str` or
//! `String` passed to `panic!` — and nothing else. The stack is gone by then:
//! unwinding has already run, and the frames it would have described no longer
//! exist.
//!
//! The only place with a view of that stack is the **panic hook**, which runs
//! at the `panic!` site before unwinding starts. So the hook captures a
//! backtrace and leaves it in a thread-local; the kernel takes it back out
//! immediately afterwards, on the same thread, and attaches it to the error.
//!
//! Without this, the error page for a panic would show the stack of the
//! machinery that caught it, which is the same for every panic and tells you
//! nothing about yours.
//!
//! # The thread-local is sound here, and it is worth saying why
//!
//! A panic hook can run on any thread, and a slot per thread means the value
//! is only ever read by the thread that wrote it. `catch_unwind` returns on
//! the thread that panicked, so the pairing holds. The value is `take`n on
//! read, so a stale backtrace cannot be attached to a later, unrelated error —
//! and if something does go wrong, the failure is a missing stack rather than
//! a wrong one.
//!
//! Async does not break it: a future's poll runs to completion on one thread,
//! and a panic during a poll unwinds through that same poll. The task can move
//! between polls, but a panic cannot span two of them.

use std::backtrace::{Backtrace, BacktraceStatus};
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::Once;

thread_local! {
    /// The stack of the most recent panic on this thread.
    static LAST_PANIC: RefCell<Option<Arc<Backtrace>>> = const { RefCell::new(None) };
}

static INSTALLED: Once = Once::new();

/// Install the hook that records panic backtraces.
///
/// Idempotent — calling it twice installs one hook. Call it once during
/// bootstrap, before the server starts:
///
/// ```no_run
/// rainier_support::panic_backtrace::install_panic_hook();
/// ```
///
/// `rainier-debug` re-exports this as `rainier_debug::install_panic_hook`,
/// which is the name an application should use — it is the crate that gives
/// the captured stack somewhere to be seen.
///
/// It **chains** to the previous hook rather than replacing it, so the usual
/// `thread 'x' panicked at ...` message still reaches stderr and anything else
/// that installed a hook still runs. A debug aid that silences the default
/// panic output would be a poor trade.
///
/// Safe to call in production: with `RUST_BACKTRACE` unset the capture is a
/// no-op and this costs one branch per panic, on a path that is already the
/// most expensive thing the process does.
pub fn install_panic_hook() {
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let captured = Backtrace::capture();
            if captured.status() == BacktraceStatus::Captured {
                let captured = Arc::new(captured);
                LAST_PANIC.with(|slot| {
                    *slot.borrow_mut() = Some(captured);
                });
            }
            previous(info);
        }));
    });
}

/// Take the backtrace of the panic that just unwound through this thread.
///
/// Returns `None` when no hook is installed, when `RUST_BACKTRACE` is unset,
/// or when it has already been taken. Call it immediately after
/// `catch_unwind` returns `Err`.
pub fn take_panic_backtrace() -> Option<Arc<Backtrace>> {
    LAST_PANIC.with(|slot| slot.borrow_mut().take())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taking_twice_yields_nothing_the_second_time() {
        // Whatever is in the slot, a take empties it. This is the property
        // that stops a stale stack being attached to an unrelated error.
        let _ = take_panic_backtrace();
        assert!(take_panic_backtrace().is_none());
    }

    #[test]
    fn the_hook_records_a_panic_when_backtraces_are_on() {
        // Only meaningful with RUST_BACKTRACE set; without it the capture is
        // deliberately a no-op and there is nothing to assert beyond "did not
        // panic while handling a panic".
        install_panic_hook();
        let _ = take_panic_backtrace();

        let result = std::panic::catch_unwind(|| panic!("deliberate"));
        assert!(result.is_err());

        let captured = take_panic_backtrace();
        if std::env::var("RUST_BACKTRACE").is_ok() {
            assert!(captured.is_some(), "the hook should have left a stack behind");
        }
        // And the slot is empty again either way.
        assert!(take_panic_backtrace().is_none());
    }

    #[test]
    fn installing_twice_is_harmless() {
        install_panic_hook();
        install_panic_hook();
        let result = std::panic::catch_unwind(|| panic!("still caught"));
        assert!(result.is_err());
    }
}
