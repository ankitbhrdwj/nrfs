// Copyright © 2019-2020 VMware, Inc. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The distributed readers-writer lock used by the replica.
//!
//! This module is only public since it needs to be exposed to the benchmarking
//! code. For clients there is no need to rely on this directly, as the RwLock
//! is embedded inside the Replica.

use core::cell::UnsafeCell;
use core::default::Default;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{spin_loop_hint, AtomicBool, AtomicUsize, Ordering};

use crossbeam_utils::CachePadded;

/// Maximum number of reader threads that this lock supports.
const MAX_READER_THREADS: usize = 192;
const_assert!(MAX_READER_THREADS > 0);

/// A scalable reader-writer lock.
///
/// This lock favours reader performance over writers. Each reader thread gets
/// its own "lock" while writers share a single lock.
///
/// `T` represents the underlying type protected by the lock.
/// Calling `read()` returns a read-guard that can be used to safely read `T`.
/// Calling `write()` returns a write-guard that can be used to safely mutate `T`.
pub struct RwLock<T>
where
    T: Sized + Default + Sync,
{
    /// The writer lock. There can be at most one writer at any given point of time.
    wlock: CachePadded<AtomicBool>,

    /// Each reader use an individual lock to access the underlying data-structure.
    rlock: [CachePadded<AtomicUsize>; MAX_READER_THREADS],

    /// The underlying data-structure.
    data: UnsafeCell<T>,

    /// Max number of hyperthreads per socket
    max_thread: usize,
}

/// A read-guard that can be used to read the underlying data structure. Writes on
/// the data structure will be blocked as long as one of these is lying around.
pub struct ReadGuard<'a, T: ?Sized + Default + Sync + 'a> {
    /// Id of the thread that acquired this guard. Required at drop time so that
    /// we can release the appropriate read lock.
    tid: usize,

    /// A reference to the Rwlock wrapping the data-structure.
    lock: &'a RwLock<T>,
}

/// A write-guard that can be used to write to the underlying data structure. All
/// reads will be blocked until this is dropped.
pub struct WriteGuard<'a, T: ?Sized + Default + Sync + 'a> {
    /// A reference to the Rwlock wrapping the data-structure.
    lock: &'a RwLock<T>,
}

impl<T> Default for RwLock<T>
where
    T: Sized + Default + Sync,
{
    /// Returns a new instance of a RwLock. Default constructs the
    /// underlying data structure.
    fn default() -> RwLock<T> {
        use arr_macro::arr;

        RwLock {
            wlock: CachePadded::new(AtomicBool::new(false)),
            rlock: arr![Default::default(); 192],
            data: UnsafeCell::new(T::default()),
            max_thread: crate::topology::MachineTopology::new()
                .cpus_on_socket(0)
                .len(),
        }
    }
}

impl<T> RwLock<T>
where
    T: Sized + Default + Sync,
{
    /// Locks the underlying data-structure for writes. The caller can retrieve
    /// a mutable reference from the returned `WriteGuard`.
    pub fn write(&self) -> WriteGuard<T> {
        let n: usize = self.max_thread;
        // First, wait until we can acquire the writer lock.
        while self.wlock.compare_and_swap(false, true, Ordering::Acquire) {
            spin_loop_hint();
        }

        // Next, wait until all readers have released their locks. This condition
        // evaluates to true if each reader lock is free (i.e equal to zero).
        while !self
            .rlock
            .iter()
            .take(n)
            .all(|item| item.load(Ordering::Relaxed) == 0)
        {
            spin_loop_hint();
        }

        unsafe { WriteGuard::new(self) }
    }

    /// Locks the underlying data-structure for reads. Allows multiple readers to acquire the lock.
    /// Blocks until there aren't any active writers.
    pub fn read(&self, tid: usize) -> ReadGuard<T> {
        // We perform a small optimization. Before attempting to acquire a read lock, we issue
        // naked reads to the write lock and wait until it is free. For that, we retrieve a
        // raw pointer to the write lock over here.
        let ptr = unsafe {
            &*(&self.wlock as *const crossbeam_utils::CachePadded<core::sync::atomic::AtomicBool>
                as *const bool)
        };

        loop {
            // First, wait until the write lock is free. This is the small
            // optimization spoken of earlier.
            unsafe {
                while core::ptr::read_volatile(ptr) {
                    spin_loop_hint();
                }
            }

            // Next, acquire this thread's read lock and actually check if the write lock
            // is free. If it is, then we're good to go because any new writers will now
            // see this acquired read lock and block. If it isn't free, then we got unlucky;
            // release the read lock and retry.
            self.rlock[tid].fetch_add(1, Ordering::Acquire);
            if !self.wlock.load(Ordering::Relaxed) {
                break;
            }

            self.rlock[tid].fetch_sub(1, Ordering::Release);
        }

        unsafe { ReadGuard::new(self, tid) }
    }

    /// Unlocks the write lock; invoked by the drop() method.
    pub(in crate::rwlock) unsafe fn write_unlock(&self) {
        if !self.wlock.compare_and_swap(true, false, Ordering::Acquire) {
            panic!("write_unlock() called without acquiring the write lock");
        }
    }

    /// Unlocks the read lock; called by the drop() method.
    pub(in crate::rwlock) unsafe fn read_unlock(&self, tid: usize) {
        if self.rlock[tid].fetch_sub(1, Ordering::Release) == 0 {
            panic!("read_unlock() called without acquiring the read lock");
        }
    }
}

impl<'rwlock, T: ?Sized + Default + Sync> ReadGuard<'rwlock, T> {
    /// Returns a read guard over a passed in reader-writer lock.
    unsafe fn new(lock: &'rwlock RwLock<T>, tid: usize) -> ReadGuard<'rwlock, T> {
        ReadGuard { tid, lock }
    }
}

impl<'rwlock, T: ?Sized + Default + Sync> WriteGuard<'rwlock, T> {
    /// Returns a write guard over a passed in reader-writer lock.
    unsafe fn new(lock: &'rwlock RwLock<T>) -> WriteGuard<'rwlock, T> {
        WriteGuard { lock }
    }
}

/// `Sync` trait allows `RwLock` to be shared between threads. The `read()` and
/// `write()` logic ensures that we will never have threads writing to and
/// reading from the underlying data structure simultaneously.
unsafe impl<T: ?Sized + Default + Sync> Sync for RwLock<T> {}

/// This `Deref` trait allows a thread to use T from a ReadGuard.
/// ReadGuard can only be dereferenced into an immutable reference.
impl<T: ?Sized + Default + Sync> Deref for ReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

/// This `Deref` trait allows a thread to use T from a WriteGuard.
/// This allows us to dereference an immutable reference.
impl<T: ?Sized + Default + Sync> Deref for WriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

/// This `DerefMut` trait allow a thread to use T from a WriteGuard.
/// This allows us to dereference a mutable reference.
impl<T: ?Sized + Default + Sync> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

/// This `Drop` trait implements the unlock logic for a reader lock. Once the `ReadGuard`
/// goes out of scope, the corresponding read lock is marked as released.
impl<T: ?Sized + Default + Sync> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        unsafe {
            let tid = self.tid;
            self.lock.read_unlock(tid);
        }
    }
}

/// This `Drop` trait implements the unlock logic for a writer lock. Once the `WriteGuard`
/// goes out of scope, the corresponding write lock is marked as released.
impl<T: ?Sized + Default + Sync> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        unsafe {
            self.lock.write_unlock();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RwLock, MAX_READER_THREADS};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::vec::Vec;

    // Tests if we can successfully default-construct a reader-writer lock.
    #[test]
    fn test_rwlock_default() {
        let lock = RwLock::<usize>::default();

        assert_eq!(lock.wlock.load(Ordering::Relaxed), false);
        for idx in 0..MAX_READER_THREADS {
            assert_eq!(lock.rlock[idx].load(Ordering::Relaxed), 0);
        }
        assert_eq!(unsafe { *lock.data.get() }, usize::default());
    }

    // Tests if the mutable reference returned on acquiring a write lock
    // can be used to write to the underlying data structure.
    #[test]
    fn test_writer_lock() {
        let lock = RwLock::<usize>::default();
        let val = 10;

        let mut guard = lock.write();
        *guard = val;

        assert_eq!(lock.wlock.load(Ordering::Relaxed), true);
        assert_eq!(lock.rlock[0].load(Ordering::Relaxed), 0);
        assert_eq!(unsafe { *lock.data.get() }, val);
    }

    // Tests if the write lock is released once a WriteGuard goes out of scope.
    #[test]
    fn test_writer_unlock() {
        let lock = RwLock::<usize>::default();

        {
            let mut _guard = lock.write();
            assert_eq!(lock.wlock.load(Ordering::Relaxed), true);
        }

        assert_eq!(lock.wlock.load(Ordering::Relaxed), false);
    }

    // Tests if the immutable reference returned on acquiring a read lock
    // can be used to read from the underlying data structure.
    #[test]
    fn test_reader_lock() {
        let lock = RwLock::<usize>::default();
        let val = 10;

        unsafe {
            *lock.data.get() = val;
        }
        let guard = lock.read(0);

        assert_eq!(lock.wlock.load(Ordering::Relaxed), false);
        assert_eq!(lock.rlock[0].load(Ordering::Relaxed), 1);
        assert_eq!(*guard, val);
    }

    // Tests if a reader lock is released once a ReadGuard goes out of scope.
    #[test]
    fn test_reader_unlock() {
        let lock = RwLock::<usize>::default();

        {
            let mut _guard = lock.read(0);
            assert_eq!(lock.rlock[0].load(Ordering::Relaxed), 1);
        }

        assert_eq!(lock.rlock[0].load(Ordering::Relaxed), 0);
    }

    // Tests that multiple readers can simultaneously acquire a readers lock
    #[test]
    fn test_multiple_readers() {
        let lock = RwLock::<usize>::default();
        let val = 10;

        unsafe {
            *lock.data.get() = val;
        }

        let f = lock.read(0);
        let s = lock.read(1);
        let t = lock.read(2);

        assert_eq!(lock.rlock[0].load(Ordering::Relaxed), 1);
        assert_eq!(lock.rlock[1].load(Ordering::Relaxed), 1);
        assert_eq!(lock.rlock[2].load(Ordering::Relaxed), 1);
        assert_eq!(*f, val);
        assert_eq!(*s, val);
        assert_eq!(*t, val);
    }

    // Tests that multiple writers and readers whose scopes don't interfere can
    // acquire the lock.
    #[test]
    fn test_lock_combinations() {
        let l = RwLock::<usize>::default();

        {
            let _g = l.write();
        }

        {
            let _g = l.write();
        }

        {
            let _f = l.read(0);
            let _s = l.read(1);
        }

        {
            let _g = l.write();
        }
    }

    // Tests that writes to the underlying data structure are atomic.
    #[test]
    fn test_atomic_writes() {
        let lock = Arc::new(RwLock::<usize>::default());
        let t = 100;

        let mut threads = Vec::new();
        for _i in 0..t {
            let l = lock.clone();
            let child = thread::spawn(move || {
                let mut ele = l.write();
                *ele += 1;
            });
            threads.push(child);
        }

        for _i in 0..threads.len() {
            let _retval = threads
                .pop()
                .unwrap()
                .join()
                .expect("Thread didn't finish successfully.");
        }

        assert_eq!(unsafe { *lock.data.get() }, t);
    }

    // Tests that the multiple readers can read from the lock in parallel.
    #[test]
    fn test_parallel_readers() {
        let lock = Arc::new(RwLock::<usize>::default());
        let t = 100;

        unsafe {
            *lock.data.get() = t;
        }

        let mut threads = Vec::new();
        for i in 0..t {
            let l = lock.clone();
            let child = thread::spawn(move || {
                let ele = l.read(i);
                assert_eq!(*ele, t);
            });
            threads.push(child);
        }

        for _i in 0..threads.len() {
            let _retval = threads
                .pop()
                .unwrap()
                .join()
                .expect("Reading didn't finish successfully.");
        }
    }

    // Tests that write_unlock() panics if called without acquiring a write lock.
    #[test]
    #[should_panic]
    fn test_writer_unlock_without_lock() {
        let lock = RwLock::<usize>::default();
        unsafe { lock.write_unlock() };
    }

    // Tests that read_unlock() panics if called without acquiring a write lock.
    #[test]
    #[should_panic]
    fn test_reader_unlock_without_lock() {
        let lock = RwLock::<usize>::default();
        unsafe { lock.read_unlock(1) };
    }

    // Tests that a read lock cannot be held along with a write lock.
    //
    // The second lock operation in this test should block indefinitely, and
    // the main thread should panic after waking up because the atomic wasn't
    // written to.
    //
    // If the main thread doesn't panic, then it means that we've got a bug
    // that allows readers to acquire the lock despite a writer already having
    // done so.
    #[test]
    #[should_panic(expected = "This test should always panic")]
    fn test_reader_after_writer() {
        let lock = RwLock::<usize>::default();
        let shared = Arc::new(AtomicUsize::new(0));

        let s = shared.clone();
        let lock_thread = thread::spawn(move || {
            let _w = lock.write();
            let _r = lock.read(0);
            s.store(1, Ordering::SeqCst);
        });

        thread::sleep(std::time::Duration::from_secs(2));
        if shared.load(Ordering::SeqCst) == 0 {
            panic!("This test should always panic");
        }
        lock_thread.join().unwrap();
    }

    // Tests that a write lock cannot be held along with a read lock.
    //
    // The second lock operation in this test should block indefinitely, and
    // the main thread should panic after waking up because the atomic wasn't
    // written to.
    //
    // If the main thread doesn't panic, then it means that we've got a bug
    // that allows writers to acquire the lock despite a reader already having
    // done so.
    #[test]
    #[should_panic(expected = "This test should always panic")]
    fn test_writer_after_reader() {
        let lock = RwLock::<usize>::default();
        let shared = Arc::new(AtomicUsize::new(0));

        let s = shared.clone();
        let lock_thread = thread::spawn(move || {
            let _r = lock.read(0);
            let _w = lock.write();
            s.store(1, Ordering::SeqCst);
        });

        thread::sleep(std::time::Duration::from_secs(2));
        if shared.load(Ordering::SeqCst) == 0 {
            panic!("This test should always panic");
        }
        lock_thread.join().unwrap();
    }

    // Tests that a write lock cannot be held along with another write lock.
    //
    // The second lock operation in this test should block indefinitely, and
    // the main thread should panic after waking up because the atomic wasn't
    // written to.
    //
    // If the main thread doesn't panic, then it means that we've got a bug
    // that allows writers to acquire the lock despite a writer already having
    // done so.
    #[test]
    #[should_panic(expected = "This test should always panic")]
    fn test_writer_after_writer() {
        let lock = RwLock::<usize>::default();
        let shared = Arc::new(AtomicUsize::new(0));

        let s = shared.clone();
        let lock_thread = thread::spawn(move || {
            let _f = lock.write();
            let _s = lock.write();
            s.store(1, Ordering::SeqCst);
        });

        thread::sleep(std::time::Duration::from_secs(2));
        if shared.load(Ordering::SeqCst) == 0 {
            panic!("This test should always panic");
        }
        lock_thread.join().unwrap();
    }
}
