use core::{ptr, task, hint, mem};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, Ordering};

mod noop {
    use core::{ptr, task};

    const VTABLE: task::RawWakerVTable = task::RawWakerVTable::new(clone, action, action, action);
    const WAKER: task::RawWaker = task::RawWaker::new(ptr::null(), &VTABLE);

    fn clone(_: *const()) -> task::RawWaker {
        WAKER
    }

    fn action(_: *const ()) {
    }

    #[inline(always)]
    pub fn waker() -> task::Waker {
        unsafe {
            task::Waker::from_raw(WAKER)
        }
    }
}

//TODO: Someone decided to change internal repr of thread for no reason so you cannot transmute it into pointer now
//Wait for https://github.com/rust-lang/rust/issues/97523 to resolve it
pub(crate) mod thread {
    use std::thread::Thread;
    use core::{task, mem};

    const VTABLE: task::RawWakerVTable = task::RawWakerVTable::new(clone, wake, wake_by_ref, on_drop);

    unsafe fn on_drop(thread: *const ()) {
        let thread = Box::from_raw(thread as *mut Thread);
        drop(thread);
    }

    unsafe fn clone(thread: *const()) -> task::RawWaker {
        let thread = Box::from_raw(thread as *mut Thread);
        let new_ptr = thread.clone();
        mem::forget(thread);
        task::RawWaker::new(Box::into_raw(new_ptr) as _, &VTABLE)
    }

    unsafe fn wake(thread: *const ()) {
        let thread = Box::from_raw(thread as *mut () as *mut Thread);
        thread.unpark();
    }

    unsafe fn wake_by_ref(thread: *const ()) {
        let thread = &*(thread as *const Thread);
        thread.unpark();
    }

    #[inline(always)]
    pub fn waker(thread: Thread) -> task::Waker {
        //double pointer is so dumb...
        let thread = Box::new(thread);
        unsafe {
            task::Waker::from_raw(task::RawWaker::new(Box::into_raw(thread) as _, &VTABLE))
        }
    }
}

/// Idle state
const WAITING: u8 = 0;

/// A new waker value is being registered with the `AtomicWaker` cell.
const REGISTERING: u8 = 0b01;

/// The waker currently registered with the `AtomicWaker` cell is being woken.
const WAKING: u8 = 0b10;

#[doc(hidden)]
/// Atomic waker used by `TimerState`
pub struct AtomicWaker {
    state: AtomicU8,
    waker: UnsafeCell<task::Waker>,
}

struct StateRestore<F: Fn()>(F);
impl<F: Fn()> Drop for StateRestore<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}

macro_rules! impl_register {
    ($this:ident($waker:ident) { $($impl:tt)+ }) => {
        match $this.state.compare_exchange(WAITING, REGISTERING, Ordering::Acquire, Ordering::Acquire).unwrap_or_else(|err| err) {
            WAITING => {
                //Make sure we do not stuck in REGISTERING state
                let state_guard = StateRestore(|| {
                    $this.state.store(WAITING, Ordering::Release);
                });

                unsafe {
                    $(
                        $impl
                    )+

                    // Release the lock. If the state transitioned to include
                    // the `WAKING` bit, this means that a wake has been
                    // called concurrently, so we have to remove the waker and
                    // wake it.`
                    //
                    // Start by assuming that the state is `REGISTERING` as this
                    // is what we jut set it to.
                    match $this.state.compare_exchange(REGISTERING, WAITING, Ordering::AcqRel, Ordering::Acquire) {
                        Ok(_) => {
                            mem::forget(state_guard);
                        }
                        Err(actual) => {
                            // This branch can only be reached if a
                            // concurrent thread called `wake`. In this
                            // case, `actual` **must** be `REGISTERING |
                            // `WAKING`.
                            debug_assert_eq!(actual, REGISTERING | WAKING);

                            let mut waker = noop::waker();
                            ptr::swap($this.waker.get(), &mut waker);

                            // Just restore,
                            // because no one could change state while state == `REGISTERING` | `WAKING`.
                            drop(state_guard);
                            waker.wake();
                        }
                    }
                }
            }
            WAKING => {
                // Currently in the process of waking the task, i.e.,
                // `wake` is currently being called on the old task handle.
                // So, we call wake on the new waker
                $waker.wake_by_ref();
                hint::spin_loop();
            }
            state => {
                // In this case, a concurrent thread is holding the
                // "registering" lock. This probably indicates a bug in the
                // caller's code as racing to call `register` doesn't make much
                // sense.
                //
                // We just want to maintain memory safety. It is ok to drop the
                // call to `register`.
                debug_assert!(
                    state == REGISTERING ||
                    state == REGISTERING | WAKING
                );
            }
        }
    };
}

impl AtomicWaker {
    ///Creates new waker set to noop
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(WAITING),
            waker: UnsafeCell::new(noop::waker()),
        }
    }

    /////This is the same function as `register` but working with owned version.
    //pub fn register(&self, waker: task::Waker) {
    //    impl_register!(self(waker) {
    //        //unconditionally store since we already have ownership
    //        *self.waker.get() = waker;
    //    });
    //}

    pub fn register_ref(&self, waker: &task::Waker) {
        impl_register!(self(waker) {
            // Lock acquired, update the waker cell
            if !(*self.waker.get()).will_wake(waker) {
                let mut waker = waker.clone();
                //Clone new waker if it is definitely not the same as old one
                ptr::swap(self.waker.get(), &mut waker);
            }
        });
    }

    pub fn wake(&self) {
        // AcqRel ordering is used in order to acquire the value of the `task`
        // cell as well as to establish a `release` ordering with whatever
        // memory the `AtomicWaker` is associated with.
        match self.state.fetch_or(WAKING, Ordering::AcqRel) {
            WAITING => {
                // The waking lock has been acquired.
                let mut waker = noop::waker();
                unsafe {
                    ptr::swap(self.waker.get(), &mut waker);
                }

                // Release the lock
                self.state.fetch_and(!WAKING, Ordering::Release);
                waker.wake();
            }
            state => {
                // There is a concurrent thread currently updating the
                // associated task.
                //
                // Nothing more to do as the `WAKING` bit has been set. It
                // doesn't matter if there are concurrent registering threads or
                // not.
                debug_assert!(
                    state == REGISTERING ||
                    state == REGISTERING | WAKING ||
                    state == WAKING
                );
            }
        }
    }
}

unsafe impl Send for AtomicWaker {}
unsafe impl Sync for AtomicWaker {}
