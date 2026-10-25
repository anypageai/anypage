//! Run IronCalc work on a thread with a large stack.
//!
//! IronCalc parses, evaluates, prints and drops formulas recursively, one
//! stack frame (or several) per level of the expression tree. Measured with
//! dependencies at opt-level 3, one level costs about 4.5 KB to parse a
//! nested function, 2 KB to evaluate a binary or unary operator, 0.5 KB to
//! print and 170 bytes to drop. `=1+1+...+1` with 560 terms (a 1 120
//! character formula) already overflows a 1 MB stack, which is the size of
//! the main thread on Windows; a stack overflow aborts the process and cannot
//! be caught.
//!
//! [`crate::formula`] bounds what one formula can need (nesting, tree depth,
//! no recursive LAMBDA), and everything that runs IronCalc's recursive code
//! goes through [`deep`], which runs it on a scoped thread whose stack is a
//! large *reservation*: the operating system commits pages only as they are
//! touched, so an ordinary edit uses a few hundred KB of it.

use std::panic::resume_unwind;
use std::sync::{Mutex, PoisonError};

/// Stack reserved for IronCalc work. The worst formula the limits in
/// [`crate::formula`] admit needs about 10 MB to evaluate (twice that with
/// IronCalc's own cross-cell recursion allowance), so this leaves a wide
/// margin, also for builds that compile IronCalc without optimisation.
#[cfg(target_pointer_width = "64")]
pub const DEEP_STACK: usize = 256 << 20;
#[cfg(not(target_pointer_width = "64"))]
pub const DEEP_STACK: usize = 48 << 20;

/// Run `f` on a thread with a [`DEEP_STACK`] stack and wait for it. A panic
/// inside `f` is resumed on the calling thread (so `module::guard` reports it
/// and a held workbook lock is poisoned as usual). If no thread can be
/// started, `f` runs on the calling thread.
pub fn deep<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    // the closure is taken exactly once: by the thread, or below if no
    // thread could be started (then the thread closure never ran)
    let slot = Mutex::new(Some(f));
    let take = || slot.lock().unwrap_or_else(PoisonError::into_inner).take();
    let ran = std::thread::scope(|s| {
        let spawned = std::thread::Builder::new().name("sheet-calc".into()).stack_size(DEEP_STACK).spawn_scoped(s, || take().map(|g| g()));
        match spawned {
            Ok(handle) => match handle.join() {
                Ok(v) => v,
                Err(panic) => resume_unwind(panic),
            },
            Err(e) => {
                log::warn!("sheet: no thread for a large stack ({e}); running on the caller's stack");
                None
            }
        }
    });
    match ran {
        Some(v) => v,
        None => match take() {
            Some(g) => g(),
            // the thread took the closure and returned nothing: impossible,
            // since a closure that ran always yields Some
            None => resume_unwind(Box::new("sheet: calculation thread lost its work")),
        },
    }
}

/// Owns a value whose drop may recurse deeply (an IronCalc model with its
/// parsed formulas) and drops it on a large stack, also on error paths.
pub struct DeepDrop<T: Send>(Option<T>);

impl<T: Send> DeepDrop<T> {
    pub fn new(v: T) -> Self {
        Self(Some(v))
    }

    /// The value; `None` only after [`DeepDrop::take`].
    pub fn get_mut(&mut self) -> Option<&mut T> {
        self.0.as_mut()
    }

    /// Hand the value over; the caller drops it from now on.
    pub fn take(mut self) -> Option<T> {
        self.0.take()
    }
}

impl<T: Send> Drop for DeepDrop<T> {
    fn drop(&mut self) {
        if let Some(v) = self.0.take() {
            deep(move || drop(v));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn depth(n: u32) -> u32 {
        // a frame of over 512 bytes that cannot be optimised into a loop
        let pad = std::hint::black_box([n as u8; 512]);
        if n == 0 {
            0
        } else {
            depth(n - 1) + 1 + u32::from(std::hint::black_box(pad)[1] > (n as u8))
        }
    }

    #[test]
    fn runs_deep_recursion_and_returns_values() {
        // 20 000 frames of over 512 bytes: more than 10 MB of stack
        assert_eq!(deep(|| depth(20_000)), 20_000);
        let mut v = vec![1, 2];
        deep(|| v.push(3));
        assert_eq!(v, [1, 2, 3]);
    }

    #[test]
    fn panics_come_back_to_the_caller() {
        let r = std::panic::catch_unwind(|| deep(|| panic!("inner")));
        assert!(r.is_err());
    }
}
