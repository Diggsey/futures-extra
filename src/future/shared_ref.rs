use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::cell::UnsafeCell;
use std::future::Future;
use std::task::{Context, Waker, Poll};
use std::pin::Pin;
use std::fmt;
use std::marker::PhantomPinned;

use futures::task::{ArcWake, waker_ref};
use slab::Slab;

const IDLE: usize = 0;
const POLLING: usize = 1;
const REPOLL: usize = 2;
const COMPLETE: usize = 3;
const POISONED: usize = 4;

const NULL_WAKER_KEY: usize = usize::max_value();

#[derive(Debug)]
enum SharedRefInner<'a, T: Future> {
    Complete(Pin<&'a T::Output>),
    Incomplete {
        owner: Pin<&'a SharedRefOwner<T>>,
        waker_key: usize,
    }
}

impl<'a, T: Future> Clone for SharedRefInner<'a, T> {
    fn clone(&self) -> Self {
        match *self {
            SharedRefInner::Complete(res) => SharedRefInner::Complete(res),
            SharedRefInner::Incomplete { owner, .. } => SharedRefInner::Incomplete { owner, waker_key: NULL_WAKER_KEY },
        }
    }
}

pub struct SharedRef<'a, T: Future>(SharedRefInner<'a, T>);

impl<'a, T: Future> fmt::Debug for SharedRef<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("SharedRef { .. }")
    }
}

impl<'a, T: Future> Clone for SharedRef<'a, T> {
    fn clone(&self) -> Self {
        SharedRef(self.0.clone())
    }
}

impl<'a, T: Future> SharedRef<'a, T> {
    /// Registers the current task to receive a wakeup when the notifier is awoken.
    fn set_waker(owner: Pin<&'a SharedRefOwner<T>>, waker_key: &mut usize, cx: &mut Context) -> Option<()> {
        let mut wakers_guard = owner.notifier.wakers.lock().unwrap();

        let wakers = wakers_guard.as_mut()?;

        if *waker_key == NULL_WAKER_KEY {
            *waker_key = wakers.insert(Some(cx.waker().clone()));
        } else {
            wakers[*waker_key] = Some(cx.waker().clone());
        }
        debug_assert!(*waker_key != NULL_WAKER_KEY);
        Some(())
    }
}

impl<'a, T: Future> Future for SharedRef<'a, T> {
    type Output = Pin<&'a T::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        match &mut self.0 {
            SharedRefInner::Complete(res) => Poll::Ready(*res),
            SharedRefInner::Incomplete { owner, waker_key } => {
                Self::set_waker(*owner, waker_key, cx);

                match owner.notifier.state.compare_and_swap(IDLE, POLLING, Ordering::SeqCst) {
                    IDLE => {
                        // Lock acquired, fall through
                    }
                    POLLING | REPOLL => {
                        // Another task is currently polling, at this point we just want
                        // to ensure that the waker for this task is registered
        
                        return Poll::Pending;
                    }
                    COMPLETE => {
                        // Complete
                        let output = unsafe { owner.output() };
                        self.0 = SharedRefInner::Complete(output);
                        return Poll::Ready(output);
                    }
                    POISONED => panic!("inner future panicked during poll"),
                    _ => unreachable!(),
                }
        
                let waker = waker_ref(&owner.notifier);
                let mut cx = Context::from_waker(&waker);
        
                struct Reset<'a>(&'a AtomicUsize);
        
                impl Drop for Reset<'_> {
                    fn drop(&mut self) {
                        use std::thread;
        
                        if thread::panicking() {
                            self.0.store(POISONED, Ordering::SeqCst);
                        }
                    }
                }
        
                let _reset = Reset(&owner.notifier.state);

                let output =  loop {
                    let future = unsafe { owner.future() };
                    let poll = future.poll(&mut cx);
        
                    match poll {
                        Poll::Pending => {
                            let state = &owner.notifier.state;
                            match state.compare_and_swap(POLLING, IDLE, Ordering::SeqCst) {
                                POLLING => {
                                    // Success
                                    return Poll::Pending;
                                }
                                REPOLL => {
                                    // Was woken since: Gotta poll again!
                                    let prev = state.swap(POLLING, Ordering::SeqCst);
                                    assert_eq!(prev, REPOLL);
                                }
                                _ => unreachable!(),
                            }
                        }
                        Poll::Ready(output) => break output,
                    }
                };
        
                unsafe {
                    *owner.value.get() = FutureOrOutput::Output(output);
                }
        
                owner.notifier.state.store(COMPLETE, Ordering::SeqCst);
                drop(_reset);

                // Wake all tasks and drop the slab
                {
                    let mut wakers_guard = owner.notifier.wakers.lock().unwrap();
                    let wakers = &mut wakers_guard.take().unwrap();
                    for (_key, opt_waker) in wakers {
                        if let Some(waker) = opt_waker.take() {
                            waker.wake();
                        }
                    }
                }

                let output = unsafe { owner.output() };
                self.0 = SharedRefInner::Complete(output);
                return Poll::Ready(output);
            }
        }
    }
}

struct Notifier {
    state: AtomicUsize,
    wakers: Mutex<Option<Slab<Option<Waker>>>>,
}

impl Notifier {
    fn new() -> Self {
        Self {
            state: AtomicUsize::new(IDLE),
            wakers: Mutex::new(Some(Slab::new()))
        }
    }
}

// Can be changed to a union when unions with non-Copy fields
// are stable.
enum FutureOrOutput<T: Future> {
    Future(T),
    Output(T::Output),
}

pub struct SharedRefOwner<T: Future> {
    value: UnsafeCell<FutureOrOutput<T>>,
    notifier: Arc<Notifier>,
    _phantom: PhantomPinned,
}

unsafe impl<T: Future> Send for SharedRefOwner<T>
where
    T: Send,
    T::Output: Send + Sync,
{}

unsafe impl<T: Future> Sync for SharedRefOwner<T>
where
    T: Send,
    T::Output: Send + Sync,
{}

impl<T: Future> fmt::Debug for SharedRefOwner<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("SharedRefOwner { .. }")
    }
}

impl<T: Future> SharedRefOwner<T> {
    pub(crate) fn new(future: T) -> Self {
        Self {
            value: UnsafeCell::new(FutureOrOutput::Future(future)),
            notifier: Arc::new(Notifier::new()),
            _phantom: PhantomPinned,
        }
    }

    pub fn mint(self: Pin<&Self>) -> SharedRef<T> {
        SharedRef(if self.notifier.state.load(Ordering::Acquire) == COMPLETE {
            SharedRefInner::Complete(unsafe { self.output() })
        } else {
            SharedRefInner::Incomplete {
                owner: self,
                waker_key: NULL_WAKER_KEY,
            }
        })
    }

    /// Must only be called when we took the lock by putting the
    /// notifier into the POLLING state.
    unsafe fn future(self: Pin<&Self>) -> Pin<&mut T> {
        Pin::new_unchecked(match &mut *self.value.get() {
            FutureOrOutput::Future(f) => f,
            _ => unreachable!(),
        })
    }
    /// Must only be called once the state is complete.
    unsafe fn output(self: Pin<&Self>) -> Pin<&T::Output> {
        Pin::new_unchecked(match &*self.value.get() {
            FutureOrOutput::Output(res) => res,
            _ => unreachable!(),
        })
    }
}

impl ArcWake for Notifier {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.state.compare_and_swap(POLLING, REPOLL, Ordering::SeqCst);

        let wakers = &mut *arc_self.wakers.lock().unwrap();
        if let Some(wakers) = wakers.as_mut() {
            for (_key, opt_waker) in wakers {
                if let Some(waker) = opt_waker.take() {
                    waker.wake();
                }
            }
        }
    }
}
