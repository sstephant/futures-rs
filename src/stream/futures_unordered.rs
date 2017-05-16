use std::boxed::Box;
use std::cell::UnsafeCell;
use std::fmt::{self, Debug};
use std::ops::Deref;
use std::sync::atomic::Ordering::{Relaxed, SeqCst, Acquire, Release, AcqRel};
use std::sync::atomic::{AtomicUsize, AtomicPtr};
use std::{mem, ptr, usize};

use {task, Stream, Future, Poll, Async, IntoFuture};
use executor::{Notify, UnsafeNotify, NotifyHandle};
use task_impl::{self, AtomicTask};

/// An unbounded queue of futures.
///
/// This "combinator" also serves a special function in this library, providing
/// the ability to maintain a queue of futures that and manage driving them all
/// to completion.
///
/// Futures are pushed into this queue and their realized values are yielded as
/// they are ready. This structure is optimized to manage a large number of
/// futures. Futures managed by `FuturesUnordered` will only be polled when they
/// generate notifications. This reduces the required amount of work needed to
/// coordinate large numbers of futures.
///
/// When a `FuturesUnordered` is first created, it does not contain any futures.
/// Calling `poll` in this state will result in `Ok(Async::Ready(None))` to be
/// returned. Futures are submitted to the queue using `push`; however, the
/// future will **not** be polled at this point. `FuturesUnordered` will only
/// poll managged futures when `FuturesUnordered::poll` is called. As such, it
/// is important to call `poll` after pushing new futures.
///
/// If `FuturesUnordered::poll` returns `Ok(Async::Ready(None))` this means that
/// the queue is currently not managing any futures. A future may be submitted
/// to the queue at a later time. At that point, a call to
/// `FuturesUnordered::poll` will either return the future's resolved value
/// **or** `Ok(Async::NotReady)` if the future has not yet completed.
///
/// Note that you can create a ready-made `FuturesUnordered` via the
/// `futures_unordered` function in the `stream` module, or you can start with a
/// blank queue with the `FuturesUnordered::new` constructor.
#[must_use = "streams do nothing unless polled"]
pub struct FuturesUnordered<F> {
    stub: Box<Node<F>>,
    inner: MyInner<F>,
    len: usize,
    head_all: *mut Node<F>,
    tail_readiness: *mut Node<F>,
}

unsafe impl<T: Send> Send for FuturesUnordered<T> {}
unsafe impl<T: Sync> Sync for FuturesUnordered<T> {}

/// Converts a list of futures into a `Stream` of results from the futures.
///
/// This function will take an list of futures (e.g. a vector, an iterator,
/// etc), and return a stream. The stream will yield items as they become
/// available on the futures internally, in the order that they become
/// available. This function is similar to `buffer_unordered` in that it may
/// return items in a different order than in the list specified.
///
/// Note that the returned queue can also be used to dynamically push more
/// futures onto the queue as they become available.
pub fn futures_unordered<I>(futures: I) -> FuturesUnordered<<I::Item as IntoFuture>::Future>
    where I: IntoIterator,
          I::Item: IntoFuture
{
    let mut queue = FuturesUnordered::new();

    for future in futures {
        queue.push(future.into_future());
    }

    return queue
}

// FuturesUnordered is implemented using two linked lists. One which links all
// futures managed by a `FuturesUnordered` and one that tracks futures that have
// been scheduled for polling. The first linked list is not thread safe and is
// only accessed by the thread that owns the `FuturesUnordered` value. The
// second linked list is an implementation of the intrusive MPSC queue algorithm
// described by 1024cores.net.
//
// When a future is submitted to the queue a node is allocated and inserted in
// both linked lists. The next call to `poll` will (eventually) see this node
// and call `poll` on the future.
//
// Before a managed future is polled, the current task's `Notify` is replaced
// with one that is aware of the specific future being run. This ensures that
// task notifications generated by that specific future are visible to
// `FuturesUnordered`. When a notification is received, the node is scheduled
// for polling by being inserted into the concurrent linked list.
//
// Each node uses an `AtomicUisze` to track it's state. The node state is the
// reference count (the number of outstanding handles to the node) as well as a
// flag tracking if the node is currently inserted in the atomic queue. When the
// future is notified, it will only insert itself into the linked list if it
// isn't currently inserted.

#[allow(missing_debug_implementations)]
struct Inner<T> {
    // The task using `FuturesUnordered`.
    parent: AtomicTask,

    // Head of the readiness queue
    head_readiness: AtomicPtr<Node<T>>,

    // Atomic ref count
    ref_count: AtomicUsize,
}

struct Node<T> {
    // The future
    future: UnsafeCell<Option<T>>,

    // Next pointer for linked list tracking all active nodes
    next_all: UnsafeCell<*mut Node<T>>,

    // Previous node in linked list tracking all active nodes
    prev_all: UnsafeCell<*mut Node<T>>,

    // Next pointer in readiness queue
    next_readiness: AtomicPtr<Node<T>>,

    // Atomic state, includes the ref count
    state: AtomicUsize,
}

enum Dequeue<T> {
    Data(*mut Node<T>),
    Empty,
    Inconsistent,
}

/// Max number of references to a single node
const MAX_REFS: usize = usize::MAX >> 1;

/// Flag tracking that a node has been queued.
const QUEUED: usize = usize::MAX - (usize::MAX >> 1);

impl<T> FuturesUnordered<T>
    where T: Future,
{
    /// Constructs a new, empty `FuturesUnordered`
    ///
    /// The returned `FuturesUnordered` does not contain any futures and, in this
    /// state, `FuturesUnordered::poll` will return `Ok(Async::Ready(None))`.
    pub fn new() -> FuturesUnordered<T> {
        let mut stub = Box::new(Node {
            future: UnsafeCell::new(None),
            next_all: UnsafeCell::new(ptr::null_mut()),
            prev_all: UnsafeCell::new(ptr::null_mut()),
            next_readiness: AtomicPtr::new(ptr::null_mut()),
            state: AtomicUsize::new(QUEUED | 1),
        });

        debug_assert!(stub.state.load(Relaxed) & QUEUED == QUEUED);

        let stub_ptr = &mut *stub as *mut _;

        let inner = Box::new(Inner {
            parent: AtomicTask::new(),
            head_readiness: AtomicPtr::new(&mut *stub as *mut _),

            // This reference count is initialized with one to be held by the
            // `FuturesUnordered` itself. It's then decremented as part of the
            // `Drop` implementation for `FuturesUnordered`.
            ref_count: AtomicUsize::new(1),
        });

        FuturesUnordered {
            stub: stub,
            len: 0,
            head_all: ptr::null_mut(),
            tail_readiness: stub_ptr,
            inner: MyInner(Box::into_raw(inner)),
        }
    }
}

impl<T> FuturesUnordered<T> {
    /// Returns the number of futures contained by the queue.
    ///
    /// This represents the total number of in-flight futures.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the queue contains no futures
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Push a future into the queue.
    ///
    /// This function submits the given future to the queue for managing. This
    /// function will not call `poll` on the submitted future. The caller must
    /// ensure that `FuturesUnordered::poll` is called in order to receive task
    /// notifications.
    pub fn push(&mut self, future: T) {
        let node = Box::new(Node {
            future: UnsafeCell::new(Some(future)),
            next_all: UnsafeCell::new(self.head_all),
            prev_all: UnsafeCell::new(ptr::null_mut()),
            next_readiness: AtomicPtr::new(ptr::null_mut()),

            // This node is initialized with a strong reference count of one
            // which is held by the internal `head_all` linked list of futures.
            //
            // This'll get decremented when the node's future is completed, or
            // the `FuturesUnordered` is dropped.
            state: AtomicUsize::new(QUEUED | 1),
        });

        let ptr = Box::into_raw(node);

        unsafe {
            if !self.head_all.is_null() {
                *(*self.head_all).prev_all.get() = ptr;
            }
        }

        self.head_all = ptr;

        // We'll need to get the future "into the system" to start tracking it,
        // e.g. getting its unpark notifications going to us tracking which
        // futures are ready. To do that we unconditionally enqueue it for
        // polling here.
        self.inner.enqueue(ptr);

        self.len += 1;
    }


    /// The dequeue function from the 1024cores intrusive MPSC queue algorithm
    fn dequeue(&mut self) -> Dequeue<T> {
        unsafe {
            // This is the 1024cores.net intrusive MPSC queue [1] "pop" function
            // with the modifications mentioned at the top of the file.
            let mut tail = self.tail_readiness;
            let mut next = (*tail).next_readiness.load(Acquire);

            if tail == self.stub() {
                if next.is_null() {
                    return Dequeue::Empty;
                }

                self.tail_readiness = next;
                tail = next;
                next = (*next).next_readiness.load(Acquire);
            }

            if !next.is_null() {
                self.tail_readiness = next;
                debug_assert!(tail != self.stub());
                return Dequeue::Data(tail);
            }

            if self.inner.head_readiness.load(Acquire) != tail {
                return Dequeue::Inconsistent;
            }

            // Push the stub node
            self.inner.enqueue(self.stub());

            next = (*tail).next_readiness.load(Acquire);

            if !next.is_null() {
                self.tail_readiness = next;
                return Dequeue::Data(tail);
            }

            Dequeue::Inconsistent
        }
    }

    unsafe fn release_node(&mut self, node: *mut Node<T>) {
        // The future is done, try to reset the queued flag. This will prevent
        // `notify` from doing any work in the future
        let prev = (*node).state.fetch_or(QUEUED, SeqCst);

        // Drop the future, even if it hasn't finished yet.
        drop((*(*node).future.get()).take());

        // Unlink the node
        self.unlink(node);

        if prev & QUEUED == 0 {
            // The queued flag has been set, this means we can safely drop the
            // node. If this doesn't happen, the node was requeued in the
            // readiness queue, so we will see it again, but next time the `&mut
            // None` branch will be hit freeing the node.
            release(node);
        }
    }

    /// Remove the node from the linked list tracking all nodes currently
    /// managed by `FuturesUnordered`.
    unsafe fn unlink(&mut self, node: *mut Node<T>) {
        let next = *(*node).next_all.get();
        let prev = *(*node).prev_all.get();
        *(*node).next_all.get() = ptr::null_mut();
        *(*node).prev_all.get() = ptr::null_mut();

        if !next.is_null() {
            *(*next).prev_all.get() = prev;
        }

        if !prev.is_null() {
            *(*prev).next_all.get() = next;
        } else {
            self.head_all = next;
        }
    }

    fn stub(&self) -> *mut Node<T> {
        &*self.stub as *const Node<T> as *mut Node<T>
    }
}

impl<T> Stream for FuturesUnordered<T>
    where T: Future
{
    type Item = T::Item;
    type Error = T::Error;

    fn poll(&mut self) -> Poll<Option<T::Item>, T::Error> {
        // Ensure `parent` is correctly set. Note that the `unsafe` here is
        // because the `park` method underneath needs mutual exclusion from
        // other calls to `park`, which we guarantee with `&mut self` above and
        // this is the only method which calls park.
        unsafe { self.inner.parent.park() };

        loop {
            let node = match self.dequeue() {
                Dequeue::Empty => {
                    if self.is_empty() {
                        return Ok(Async::Ready(None));
                    } else {
                        return Ok(Async::NotReady)
                    }
                }
                Dequeue::Inconsistent => {
                    // At this point, it may be worth yielding the thread &
                    // spinning a few times... but for now, just yield using the
                    // task system.
                    task::current().notify();
                    return Ok(Async::NotReady);
                }
                Dequeue::Data(node) => node,
            };

            debug_assert!(node != self.stub());

            unsafe {
                // If the future has already gone away then we're just cleaning
                // out this node.
                if (*(*node).future.get()).is_none() {
                    assert!((*(*node).next_all.get()).is_null());
                    assert!((*(*node).prev_all.get()).is_null());
                    release(node);
                    continue
                }

                // Unset queued flag... this must be done before
                // polling. This ensures that the future gets
                // rescheduled if it is notified **during** a call
                // to `poll`.
                let prev = (*node).state.fetch_and(!QUEUED, SeqCst);
                assert!(prev & QUEUED == QUEUED);

                // Poll the underlying future with the appropriate `notify`
                // implementation and `id`. This is where a large bit of the
                // unsafety starts to stem from internally. The `notify`
                // instance itself is basically just our `*mut Inner<T>` and
                // tracks the mpsc queue of ready futures. The `id`, however, is
                // the `*mut Node<T>` cast to a `u64`.
                //
                // We then override the `ref_inc` and `ref_dec` functions below
                // in `Notify for Inner<T>` to track the reference count of the
                // `*mut Node<T>`.
                //
                // Critically though neither `Inner<T>` nor `Node<T>` will
                // actually access `T`, the future, while they're floating
                // around inside of `Task` instances. These structs will
                // basically just use `T` to size the internal allocation,
                // appropriately accessing fields and deallocating the node if
                // need be.
                //
                // You can sort of think of `*mut Node<T>` as a `Weak<T>`, but
                // not exactly because we statically know that we won't attempt
                // to upgrade it, hence the looser restrictions around safety
                // here.
                let id = node as u64;
                let res = task_impl::with_notify(&self.inner, id, || {
                    let future = (*node).future.get();
                    (*future).as_mut().unwrap().poll()
                });

                let ret = match res {
                    Ok(Async::NotReady) => continue,
                    Ok(Async::Ready(e)) => Ok(Async::Ready(Some(e))),
                    Err(e) => Err(e),
                };
                self.len -= 1;
                self.release_node(node);

                return ret
            }
        }
    }
}

impl<T: Debug> Debug for FuturesUnordered<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "FuturesUnordered {{ ... }}")
    }
}

impl<T> Drop for FuturesUnordered<T> {
    fn drop(&mut self) {
        // When a `FuturesUnordered` is dropped we want to drop all futures associated
        // with it. At the same time though there may be tons of `Task` handles
        // flying around which contain `Node<T>` references inside them. We'll
        // let those naturally get deallocated when the `Task` itself goes out
        // of scope or gets notified.
        //
        // Note that the `inner.drop_raw()` here is dropping our own reference
        // count of `inner`, it may not get deallocated until later as well.
        unsafe {
            while !self.head_all.is_null() {
                let head = self.head_all;
                self.release_node(head);
            }

            (*self.inner).drop_raw();
        }
    }
}

#[allow(missing_debug_implementations)]
struct MyInner<T>(*mut Inner<T>);

impl<T> Deref for MyInner<T> {
    type Target = Inner<T>;

    fn deref(&self) -> &Inner<T> {
        unsafe { &*self.0 }
    }
}

impl<T> Clone for MyInner<T> {
    fn clone(&self) -> MyInner<T> {
        unsafe {
            mem::forget((*self.0).clone_raw());
        }
        MyInner(self.0)
    }
}

impl<T> From<MyInner<T>> for NotifyHandle {
    fn from(me: MyInner<T>) -> NotifyHandle {
        unsafe {
            let handle = NotifyHandle::new(hide_lt(me.0));
            mem::forget(me);
            return handle
        }
    }
}

impl<T> Drop for MyInner<T> {
    fn drop(&mut self) {
        unsafe {
            (*self.0).drop_raw()
        }
    }
}

impl<T> Inner<T> {
    /// The enqueue function from the 1024cores intrusive MPSC queue algorithm.
    fn enqueue(&self, node: *mut Node<T>) {
        unsafe {
            debug_assert!((*node).state.load(Relaxed) & QUEUED == QUEUED);

            // This action does not require any coordination
            (*node).next_readiness.store(ptr::null_mut(), Relaxed);

            // Note that these atomic orderings come from 1024cores
            let prev = self.head_readiness.swap(node, AcqRel);
            (*prev).next_readiness.store(node, Release);
        }
    }
}

impl<T> Notify for Inner<T> {
    fn notify(&self, id: u64) {
        unsafe {
            let node = Node::<T>::from_id(id);

            // It's our job to notify the node that it's ready to get polled,
            // meaning that we need to enqueue it into the readiness queue. To
            // do this we flag that we're ready to be queued, and if successful
            // we then do the literal queueing operation, ensuring that we're
            // only queued once.
            //
            // Once the node is inserted we be sure to notify the parent task,
            // as it'll want to come along and pick up our node now.
            let prev = (*node).state.fetch_or(QUEUED, SeqCst);
            if prev & QUEUED == 0 {
                self.enqueue(node);
                self.parent.notify();
            }
        }
    }

    fn ref_inc(&self, id: u64) {
        unsafe {
            let node = Node::<T>::from_id(id);

            // This is basically the same as Arc::clone, and see Arc::clone for
            // rationale on the Relaxed fetch_add
            let old_size = (*node).state.fetch_add(1, Relaxed);
            if old_size > MAX_REFS {
                abort("refcount overflow");
            }
        }
    }

    fn ref_dec(&self, id: u64) {
        unsafe {
            let node = Node::<T>::from_id(id);
            release(node);
        }
    }
}

unsafe impl<T> UnsafeNotify for Inner<T> {
    unsafe fn clone_raw(&self) -> NotifyHandle {
        // This is basically the same as Arc::clone, and see Arc::clone for
        // rationale on the Relaxed fetch_add
        let old_size = self.ref_count.fetch_add(1, Relaxed);
        if old_size > MAX_REFS {
            abort("refcount overflow");
        }

        NotifyHandle::new(hide_lt(self))
    }

    unsafe fn drop_raw(&self) {
        if self.ref_count.fetch_sub(1, SeqCst) != 1 {
            return;
        }

        ptr::drop_in_place(self as *const Inner<T> as *mut Inner<T>);
    }
}

// Note that these are all basically a lie. The safety here, though, derives
// from how `Inner<T>` will never touch `T` in terms of memory, drops, etc. We
// basically only use it to statically know the size of the `Node<T>` instances
// that we are dropping.
unsafe impl<T> Send for Inner<T> {}
unsafe impl<T> Sync for Inner<T> {}

unsafe fn hide_lt<T>(p: *const Inner<T>) -> *mut UnsafeNotify {
    mem::transmute(p as *mut Inner<T> as *mut UnsafeNotify)
}

impl<T> Node<T> {
    unsafe fn from_id(id: u64) -> *mut Node<T> {
        id as *mut Node<T>
    }
}

// Note that this function needs to critically *be blind to T*. This can run on
// any thread or in any lifetime, irrespective to `T` itself and whether it
// would safely allow that. As a result it's critical this function doesn't
// access `T` at all via dtor, deref, etc.
unsafe fn release<T>(node: *mut Node<T>) {
    let old_state = (*node).state.fetch_sub(1, SeqCst);

    if (old_state & !QUEUED) != 1 {
        return;
    }

    // The future should have already been cleared, and if not we're not allowed
    // to touch `T` so we need to abort.
    if (*(*node).future.get()).is_some() {
        abort("future should already be dropped");
    }

    drop(Box::from_raw(node));
}

fn abort(s: &str) -> ! {
    struct DoublePanic;

    impl Drop for DoublePanic {
        fn drop(&mut self) {
            panic!("panicking twice to abort the program");
        }
    }

    let _bomb = DoublePanic;
    panic!("{}", s);

}
