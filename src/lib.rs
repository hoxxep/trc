//! `Trc` is a performant heap-allocated smart pointer that implements a thread reference counting.
//! `Trc` stands for: Thread Reference Counted.
//! [`Trc`] provides shared ownership of the data similar to `Arc<T>` and `Rc<T>`.
//! It implements thread reference counting, which is based on the observation that most objects are only used by one thread.
//! This means that two reference counts can be created: one for local thread use, and one atomic one for sharing between threads.
//! Thread reference counting sets the atomic reference count to the number of threads using the data.
//!
//! ## Breaking reference cycles with `Weak<T>`
//! A cycle between `Trc` pointers cannot be deallocated as the reference counts will never reach zero. The solution is a `Weak<T>`.
//! A `Weak<T>` is a non-owning reference to the data held by a `Trc<T>`.
//! They break reference cycles by adding a layer of indirection and act as an observer. They cannot even access the data directly, and
//! must be converted back into `Trc<T>`. `Weak<T>` does not keep the value alive (which can be dropped), and only keeps the backing allocation alive.
//! See [`Weak`] for more information.
//!
//! ## Sending data across threads with `SharedTrc<T>`
//! To soundly implement thread safety `Trc<T>` does not itself implement [`Send`] or [`Sync`].
//! However, `SharedTrc<T>` does, and it is the only way to safely send a `Trc<T>` across
//! threads. See [`SharedTrc`] for it's API, which is similar to that of `Weak`.
//! See [`SharedTrc`] for it's API, which is similar to that of [`Weak`].

#[cfg(test)]
mod tests;

#[cfg(not(target_has_atomic = "ptr"))]
compile_error!("Cannot use `Trc` on a system without atomics.");

use std::{
    alloc::{alloc, Layout},
    borrow::Borrow,
    error::Error,
    fmt::{Debug, Display, Pointer},
    hash::{Hash, Hasher},
    mem::{forget, ManuallyDrop, MaybeUninit},
    ops::Deref,
    os::fd::{AsFd, AsRawFd},
    panic::UnwindSafe,
    pin::Pin,
    ptr::{self, addr_of, addr_of_mut, slice_from_raw_parts_mut, write, NonNull}
};

use core::sync::atomic::AtomicUsize;

const MAX_REFCOUNT: usize = (isize::MAX) as usize;

#[repr(C)]
struct SharedTrcInternal<T: ?Sized> {
    atomicref: AtomicUsize,
    weakcount: AtomicUsize,
    data: T,
}

/// `Trc` is a performant heap-allocated smart pointer that implements thread reference counting.
/// `Trc` stands for: Thread Reference Counted.
/// `Trc` provides shared ownership of the data similar to `Arc<T>` and `Rc<T>`.
/// It implements thread reference counting, which is based on the observation that most objects are only used by one thread.
/// This means that two reference counts can be created: one for local thread use, and one atomic one for sharing between threads.
/// Thread reference counting sets the atomic reference count to the number of threads using the data.
///
/// ## Breaking reference cycles with `Weak<T>`
/// A cycle between `Trc` pointers cannot be deallocated as the reference counts will never reach zero. The solution is a `Weak<T>`.
/// A `Weak<T>` is a non-owning reference to the data held by a `Trc<T>`.
/// They break reference cycles by adding a layer of indirection and act as an observer. They cannot even access the data directly, and
/// must be converted back into `Trc<T>`. `Weak<T>` does not keep the value alive (which can be dropped), and only keeps the backing allocation alive.
/// See [`Weak`] for more information.
///
/// ## Sending data across threads with `SharedTrc<T>`
/// To soundly implement thread safety `Trc<T>` does not implement [`Send`] or [`Sync`].
/// However, [`SharedTrc`] does, and it is the only way to safely send a `Trc<T>` across threads.
/// See [`SharedTrc`] for it's API, which is similar to that of [`Weak`].
///
/// ## Clone behavior
/// When a `Trc<T>` is cloned, it's internal (wrapped) data stays at the same memory location, but a new `Trc<T>` is constructed and returned.
/// This makes a `clone` a relatively inexpensive operation because only a wrapper is constructed.
/// This new `Trc<T>` points to the same memory, and all `Trc<T>`s that point to that memory in that thread will have their local thread reference counts incremented
/// and their atomic reference counts unchanged.
///
/// To soundly implement thread safety `Trc<T>` does not itself implement [`Send`] or [`Sync`]. However, `SharedTrc<T>` does, and it is the only way to safely send a `Trc<T>` across
/// threads. See [`SharedTrc`] for it's API, which is similar to that of `Weak`.
///
/// ## Drop behavior
///
/// When a `Trc<T>` is dropped the local thread reference count is decremented. If it is zero, the atomic reference count is also decremented.
/// If the atomic reference count is zero, then the internal data is dropped. Regardless of whether the atomic reference count is zero, the
/// local `Trc<T>` is dropped.
///
/// ## [`Deref`] and `DerefMut` behavior
/// For ease of developer use, `Trc<T>` comes with [`Deref`] implemented.
/// `Trc<T>` automatically dereferences to `&T`. This allows method calls and member access of `T`.
/// `DerefMut` is not directly implemented as that could cause UB due to the possibility of multiple `&mut` references to the `Trc`.
/// To prevent name clashes, `Trc<T>`'s functions are associated.
///
/// ## Footnote on `dyn` wrapping
/// Rust's limitations mean that `Trc` will not be able to be used as a method receiver wrapper until
/// CoerceUnsized, and Receiver (with arbitrary_self_types) are stablized. However, DispatchFromDyn cannot be implemented due
/// to the requirements of thread reference counting, and so `Trc` will not be able to be used as a trait object method receiver.
/// As an alternative, one can use a [`Box`] as a wrapper and then wrap with `Trc<T>`.
///
/// ## Examples
///
/// Example in a single thread:
/// ```
/// use trc::Trc;
///
/// let mut trc = Trc::new(100);
/// assert_eq!(*trc, 100);
/// *Trc::get_mut(&mut trc).unwrap() = 200;
/// assert_eq!(*trc, 200);
/// ```
///
/// Example with multiple threads:
/// ```
/// use std::thread;
/// use trc::Trc;
/// use trc::SharedTrc;
///
/// let trc = Trc::new(100);
/// let shared = SharedTrc::from_trc(&trc);
/// let handle = thread::spawn(move || {
///     let mut trc = SharedTrc::to_trc(shared);
/// });
///
/// handle.join().unwrap();
/// assert_eq!(*trc, 100);
/// ```
///
pub struct Trc<T: ?Sized> {
    shared: NonNull<SharedTrcInternal<T>>,
    threadref: NonNull<usize>,
}

/// `SharedTrc<T>` is a thread-safe wrapper used to send `Trc<T>`s across threads.
/// Unlike [`Trc`] (which is `!Send` and `!Sync`), `SharedTrc` is [`Send`] and [`Sync`]. This means that along with
/// [`Weak`], `SharedTrc` is one of the ways to send a `Trc` across threads. However, unlike `Weak`, `SharedTrc` does not
/// modify the weak count - and only modifies the strong count. In addition, `SharedTrc` will not fail on conversion
/// back to a `Trc` because it prevents the data `T` from being dropped.
///
/// ## Examples
///
/// Example in a single thread:
/// ```
/// use trc::Trc;
/// use trc::SharedTrc;
///
/// let trc = Trc::new(String::from("Trc"));
/// let shared: SharedTrc<_> = (&trc).into();
/// let trc2: Trc<String> = shared.into();
/// assert_eq!(*trc, *trc2);
/// ```
///
/// See [`Trc`] or [`Weak`] for an example with multiple threads.
pub struct SharedTrc<T: ?Sized> {
    data: NonNull<SharedTrcInternal<T>>,
}

/// `Weak<T>` is a non-owning reference to `Trc<T>`'s data. It is used to prevent cyclic references which cause memory to never be freed.
/// `Weak<T>` does not keep the value alive (which can be dropped), they only keep the backing allocation alive. `Weak<T>` cannot even directly access the memory,
/// and must be converted into `Trc<T>` to do so.
///
/// One use case of a `Weak<T>`
/// is to create a tree: The parent nodes own the child nodes, and have strong `Trc<T>` references to their children. However, their children have `Weak<T>` references
/// to their parents.
///
/// To prevent name clashes, `Weak<T>`'s functions are associated.
///
/// ## Examples
///
/// Example in a single thread:
/// ```
/// use trc::Trc;
/// use trc::Weak;
///
/// let trc = Trc::new(100);
/// let weak = Trc::downgrade(&trc);
/// let new_trc = Weak::upgrade(&weak).unwrap();
/// assert_eq!(*new_trc, 100);
/// ```
///
/// Example with multiple threads:
/// ```
/// use std::thread;
/// use trc::Trc;
/// use trc::SharedTrc;
///
/// let trc = Trc::new(100);
/// let shared = SharedTrc::from_trc(&trc);
/// let handle = thread::spawn(move || {
///     let mut trc = SharedTrc::to_trc(shared);
///     assert_eq!(*trc, 100);
/// });
/// handle.join().unwrap();
/// assert_eq!(*trc, 100);
/// ```
///
pub struct Weak<T: ?Sized> {
    data: NonNull<SharedTrcInternal<T>>,
}

unsafe impl<T: Sync + Send> Send for SharedTrc<T> {}
unsafe impl<T: Sync + Send> Sync for SharedTrc<T> {}

impl<T: ?Sized> SharedTrc<T> {
    /// Convert a `Trc<T>` to a `SharedTrc<T>`, incrementing it's atomic reference count.
    /// While this `SharedTrc<T>` is alive, the data contained by `Trc<T>` will not be dropped, which is
    /// unlike a `Weak<T>`.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let trc = Trc::new(100);
    /// let shared = SharedTrc::from_trc(&trc);
    /// ```
    #[inline]
    pub fn from_trc(trc: &Trc<T>) -> Self {
        let prev = sum_value(
            &unsafe { trc.shared.as_ref() }.atomicref,
            1,
            core::sync::atomic::Ordering::Acquire,
        );
        if prev > MAX_REFCOUNT {
            panic!("Overflow of maximum strong reference count.");
        }
        SharedTrc { data: trc.shared }
    }

    /// Convert a `SharedTrc<T>` to a `Trc<T>`. To prevent memory leaks, this function takes
    /// ownership of the `SharedTrc`. Unlike `Weak::to_trc`, this function will not fail as it
    /// prevents the data from being dropped.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let trc = Trc::new(100);
    /// let shared = SharedTrc::from_trc(&trc);
    /// drop(trc);
    /// let trc2 = SharedTrc::to_trc(shared);
    /// ```
    pub fn to_trc(this: Self) -> Trc<T> {
        let tbx = Box::new(1);
        let res = Trc {
            threadref: NonNull::from(Box::leak(tbx)),
            shared: this.data,
        };
        core::mem::forget(this);
        res
    }

    /// Return the atomic reference count of the object. This is how many threads are using the data referenced by this `SharedTrc<T>`.
    ///
    /// # Examples
    /// ```
    /// use std::thread;
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let trc = Trc::new(100);
    /// let shared = SharedTrc::from_trc(&trc);
    ///
    /// let handle = thread::spawn(move || {
    ///     assert_eq!(SharedTrc::atomic_count(&shared), 2);
    ///     let trc = SharedTrc::to_trc(shared);
    /// });
    ///
    /// handle.join().unwrap();
    /// assert_eq!(*trc, 100);
    /// ```
    #[inline]
    pub fn atomic_count(this: &Self) -> usize {
        unsafe { this.data.as_ref() }
            .atomicref
            .load(core::sync::atomic::Ordering::Relaxed)
    }
}

impl<T: ?Sized> Clone for SharedTrc<T> {
    /// Clone a `SharedTrc<T>` (increment the strong count).
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let trc = Trc::new(100);
    /// let shared1 = SharedTrc::from_trc(&trc);
    /// let shared2 = shared1.clone();
    /// assert_eq!(SharedTrc::atomic_count(&shared1), 3);
    /// ```
    #[inline]
    fn clone(&self) -> Self {
        let prev = sum_value(
            &unsafe { self.data.as_ref() }.atomicref,
            1,
            core::sync::atomic::Ordering::AcqRel,
        );
        if prev > MAX_REFCOUNT {
            panic!("Overflow of maximum strong reference count.");
        }
        SharedTrc { data: self.data }
    }
}

impl<T: ?Sized> Drop for SharedTrc<T> {
    #[inline]
    fn drop(&mut self) {
        if sub_value(
            unsafe { &(*self.data.as_ptr()).atomicref },
            1,
            core::sync::atomic::Ordering::Release,
        ) != 1
        {
            return;
        }

        let weak =
            unsafe { &(*self.data.as_ptr()).weakcount }.load(core::sync::atomic::Ordering::Acquire);
        if weak == 1 {
            core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
            unsafe { core::ptr::drop_in_place(addr_of_mut!((*self.data.as_ptr()).data)) };
            Weak { data: self.data };
        }
    }
}

impl<T: ?Sized> From<SharedTrc<T>> for Trc<T> {
    /// Convert a `SharedTrc<T>` to a `Trc<T>`. To prevent memory leaks, this function takes
    /// ownership of the `SharedTrc`. Unlike `Weak::to_trc`, this function will not fail as it
    /// prevents the data from being dropped.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let trc = Trc::new(100);
    /// let shared = SharedTrc::from_trc(&trc);
    /// drop(trc);
    /// let trc2 = SharedTrc::to_trc(shared);
    /// ```
    fn from(value: SharedTrc<T>) -> Self {
        SharedTrc::to_trc(value)
    }
}

impl<T: ?Sized> From<&Trc<T>> for SharedTrc<T> {
    /// Convert a `Trc<T>` to a `SharedTrc<T>`, incrementing it's atomic reference count.
    /// While this `SharedTrc<T>` is alive, the data contained by `Trc<T>` will not be dropped, which is
    /// unlike a `Weak<T>`.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let trc = Trc::new(100);
    /// let shared = SharedTrc::from_trc(&trc);
    /// ```
    fn from(value: &Trc<T>) -> Self {
        SharedTrc::from_trc(value)
    }
}

impl<T: ?Sized> From<Trc<T>> for SharedTrc<T> {
    /// Convert a `Trc<T>` to a `SharedTrc<T>`, incrementing it's atomic reference count.
    /// While this `SharedTrc<T>` is alive, the data contained by `Trc<T>` will not be dropped, which is
    /// unlike a `Weak<T>`.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let trc = Trc::new(100);
    /// let shared = SharedTrc::from_trc(&trc);
    /// ```
    fn from(value: Trc<T>) -> Self {
        SharedTrc::from_trc(&value)
    }
}

impl<T: ?Sized> SharedTrc<T> {
    /// Return the weak count of the object. This is how many weak counts - across all threads - are pointing to the allocation inside of `SharedTrc<T>`.
    /// It includes the implicit weak reference held by all `SharedTrc<T>` to themselves.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    /// use trc::Weak;
    ///
    /// let trc = Trc::new(100i32);
    /// let weak = Trc::downgrade(&trc);
    /// let weak2 = Trc::downgrade(&trc);
    /// let new_trc = Weak::upgrade(&weak).expect("Value was dropped");
    /// drop(weak);
    /// let shared: SharedTrc<_> = trc.into();
    /// assert_eq!(SharedTrc::weak_count(&shared), 2);
    /// ```
    #[inline]
    pub fn weak_count(this: &Self) -> usize {
        unsafe { this.data.as_ref() }
            .weakcount
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    /// Checks if the other `SharedTrc<T>` is equal to this one according to their internal pointers.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let trc1 = Trc::new(100);
    /// let trc2 = trc1.clone();
    /// let shared1: SharedTrc<_> = trc1.into();
    /// let shared2: SharedTrc<_> = trc2.into();
    /// assert!(SharedTrc::ptr_eq(&shared1, &shared2));
    /// ```
    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.data.as_ptr() == other.data.as_ptr()
    }

    /// Gets the raw pointer to the most inner layer of `SharedTrc<T>`.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let trc = Trc::new(100);
    /// assert_eq!(SharedTrc::as_ptr(&SharedTrc::from_trc(&trc)), Trc::as_ptr(&trc))
    /// ```
    #[inline]
    pub fn as_ptr(this: &Self) -> *const T {
        let sharedptr = NonNull::as_ptr(this.data);
        unsafe { addr_of_mut!((*sharedptr).data) }
    }

    /// Converts a `SharedTrc<T>` into `*const T`, without freeing the allocation.
    /// To avoid a memory leak, be sure to call `from_raw` to reclaim the allocation.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let shared: SharedTrc<_> = Trc::new(100).into();
    /// let ptr = SharedTrc::into_raw(shared);
    ///
    /// assert_eq!(unsafe { *ptr }, 100);
    ///
    /// unsafe { Trc::from_raw(ptr) };
    /// ```
    pub fn into_raw(this: Self) -> *const T {
        let ptr = Self::as_ptr(&this);

        forget(this);
        ptr
    }
}

impl<T> SharedTrc<T> {
    /// Converts a `*const T` into `SharedTrc<T>`. The caller must uphold the below safety constraints.
    /// To avoid a memory leak, be sure to call `from_raw` to reclaim the allocation.
    ///
    /// # Safety
    /// - The given pointer must be a valid pointer to `T` that came from `into_raw`.
    /// - After `from_raw`, the pointer must not be accessed.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let shared: SharedTrc<_> = Trc::new(100).into();
    /// let ptr = SharedTrc::into_raw(shared);
    ///
    /// assert_eq!(unsafe { *ptr }, 100);
    ///
    /// unsafe { Trc::from_raw(ptr) };
    /// ```
    pub unsafe fn from_raw(ptr: *const T) -> Self {
        let layout = Layout::new::<SharedTrcInternal<()>>();
        let n = layout.size();

        let data_ptr = (ptr as *const u8).sub(n) as *mut SharedTrcInternal<T>;

        SharedTrc {
            data: NonNull::new_unchecked(data_ptr),
        }
    }
}

impl<T: ?Sized> Deref for SharedTrc<T> {
    type Target = T;

    /// Get an immutable reference to the internal data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    /// use std::ops::Deref;
    ///
    /// let mut shared: SharedTrc<_> = Trc::new(100i32).into();
    /// assert_eq!(*shared, 100i32);
    /// assert_eq!(shared.deref(), &100i32);
    /// ```
    #[inline]
    fn deref(&self) -> &Self::Target {
        &unsafe { self.data.as_ref() }.data
    }
}

impl<T: ?Sized> AsRef<T> for SharedTrc<T> {
    fn as_ref(&self) -> &T {
        SharedTrc::deref(self)
    }
}

impl<T: ?Sized> Borrow<T> for SharedTrc<T> {
    fn borrow(&self) -> &T {
        self.as_ref()
    }
}

impl<T: ?Sized + Default> Default for SharedTrc<T> {
    fn default() -> Self {
        Self::from_trc(&Trc::new(Default::default()))
    }
}

impl<T: Display> Display for SharedTrc<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt((*self).deref(), f)
    }
}

impl<T: Debug> Debug for SharedTrc<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt((*self).deref(), f)
    }
}

impl<T: ?Sized> Pointer for SharedTrc<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Pointer::fmt(&addr_of!(unsafe { self.data.as_ref() }.data), f)
    }
}

impl<T: Hash> Hash for SharedTrc<T> {
    /// Pass the data contained in this `SharedTrc<T>` to the provided hasher.
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.deref().hash(state);
    }
}

impl<T: PartialOrd> PartialOrd for SharedTrc<T> {
    /// "Greater than or equal to" comparison for two `SharedTrc<T>`s.
    ///
    /// Calls `.ge` on the data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let shared1: SharedTrc<_> = Trc::from(100).into();
    /// let shared2 = shared1.clone();
    /// assert!(shared1>=shared2);
    /// ```
    #[inline]
    fn ge(&self, other: &Self) -> bool {
        self.deref().ge(other.deref())
    }

    /// "Less than or equal to" comparison for two `SharedTrc<T>`s.
    ///
    /// Calls `.le` on the data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let shared1: SharedTrc<_> = Trc::from(100).into();
    /// let shared2 = shared1.clone();
    /// assert!(shared1<=shared2);
    /// ```
    #[inline]
    fn le(&self, other: &Self) -> bool {
        self.deref().ge(other.deref())
    }

    /// "Greater than" comparison for two `SharedTrc<T>`s.
    ///
    /// Calls `.gt` on the data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let shared1: SharedTrc<_> = Trc::from(200).into();
    /// let shared2: SharedTrc<_> = Trc::from(100).into();
    /// assert!(shared1>shared2);
    /// ```
    #[inline]
    fn gt(&self, other: &Self) -> bool {
        self.deref().gt(other.deref())
    }

    /// "Less than" comparison for two `SharedTrc<T>`s.
    ///
    /// Calls `.lt` on the data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let shared1: SharedTrc<_> = Trc::from(100).into();
    /// let shared2: SharedTrc<_> = Trc::from(200).into();
    /// assert!(shared1<shared2);
    /// ```
    #[inline]
    fn lt(&self, other: &Self) -> bool {
        self.deref().lt(other.deref())
    }

    /// Partial comparison for two `SharedTrc<T>`s.
    ///
    /// Calls `.partial_cmp` on the data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    /// use std::cmp::Ordering;
    ///
    /// let shared1: SharedTrc<_> = Trc::from(100).into();
    /// let shared2: SharedTrc<_> = Trc::from(200).into();
    /// assert_eq!(Some(Ordering::Less), shared1.partial_cmp(&shared2));
    /// ```
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        self.deref().partial_cmp(other.deref())
    }
}

impl<T: Ord> Ord for SharedTrc<T> {
    /// Comparison for two `SharedTrc<T>`s. The two are compared by calling `.cmp` on the inner values.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    /// use std::cmp::Ordering;
    ///
    /// let shared1: SharedTrc<_> = Trc::from(100).into();
    /// let shared2: SharedTrc<_> = Trc::from(200).into();
    /// assert_eq!(Ordering::Less, shared1.cmp(&shared2));
    /// ```
    #[inline]
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.deref().cmp(other.deref())
    }
}

impl<T: Eq> Eq for SharedTrc<T> {}

impl<T: PartialEq> PartialEq for SharedTrc<T> {
    /// Equality by value comparison for two `SharedTrc<T>`s, even if the data is in different allocoations.
    ///
    /// Calls `.eq` on the data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let shared1: SharedTrc<_> = Trc::from(100).into();
    /// let shared2: SharedTrc<_> = Trc::from(100).into();
    /// assert!(shared1==shared2);
    /// ```
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.deref().eq(other.deref())
    }

    /// Inequality by value comparison for two `SharedTrc<T>`s, even if the data is in different allocoations.
    ///
    /// Calls `.ne` on the data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let shared1: SharedTrc<_> = Trc::from(100).into();
    /// let shared2: SharedTrc<_> = Trc::from(200).into();
    /// assert!(shared1!=shared2);
    /// ```
    #[allow(clippy::partialeq_ne_impl)]
    #[inline]
    fn ne(&self, other: &Self) -> bool {
        self.deref().ne(other.deref())
    }
}

impl<T: AsFd> AsFd for SharedTrc<T> {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        (**self).as_fd()
    }
}

impl<T: AsRawFd> AsRawFd for SharedTrc<T> {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        (**self).as_raw_fd()
    }
}

#[allow(deprecated)]
impl<T: Error> Error for SharedTrc<T> {
    fn cause(&self) -> Option<&dyn Error> {
        (**self).cause()
    }
    fn description(&self) -> &str {
        (**self).description()
    }
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        (**self).source()
    }
}

impl<T: ?Sized> Unpin for SharedTrc<T> {}
impl<T: ?Sized> UnwindSafe for SharedTrc<T> {}

#[inline(always)]
fn sum_value(value: &AtomicUsize, offset: usize, ordering: core::sync::atomic::Ordering) -> usize {
    value.fetch_add(offset, ordering)
}

#[inline(always)]
fn sub_value(value: &AtomicUsize, offset: usize, ordering: core::sync::atomic::Ordering) -> usize {
    value.fetch_sub(offset, ordering)
}

impl<T> Trc<T> {
    /// Creates a new `Trc<T>` from the provided data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let trc = Trc::new(100);
    /// assert_eq!(*trc, 100);
    /// ```
    #[inline]
    pub fn new(value: T) -> Self {
        let shareddata = SharedTrcInternal {
            atomicref: AtomicUsize::new(1),
            weakcount: AtomicUsize::new(1),
            data: value,
        };

        let sbx = Box::new(shareddata);

        let tbx = Box::new(1);

        Trc {
            threadref: NonNull::from(Box::leak(tbx)),
            shared: NonNull::from(Box::leak(sbx)),
        }
    }

    /// Creates a new uninitialized `Trc<T>`.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let mut trc = Trc::new_uninit();
    ///
    /// Trc::get_mut(&mut trc).unwrap().write(5);
    ///
    /// let five = unsafe { trc.assume_init() };
    ///
    /// assert_eq!(*five, 5);
    /// ```
    #[inline]
    pub fn new_uninit() -> Trc<MaybeUninit<T>> {
        let shareddata = SharedTrcInternal {
            atomicref: AtomicUsize::new(1),
            weakcount: AtomicUsize::new(1),
            data: MaybeUninit::<T>::uninit(),
        };

        let sbx = Box::new(shareddata);

        let tbx = Box::new(1);

        Trc {
            threadref: NonNull::from(Box::leak(tbx)),
            shared: NonNull::from(Box::leak(sbx)),
        }
    }

    /// Creates a new cyclic `Trc<T>` from the provided data. It allows the storage of `Weak<T>` which points the the allocation
    /// of `Trc<T>`inside of `T`. Holding a `Trc<T>` inside of `T` would cause a memory leak. This method works around this by
    /// providing a `Weak<T>` during the construction of the `Trc<T>`, so that the `T` can store the `Weak<T>` internally.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::Weak;
    ///
    /// struct T(Weak<T>);
    ///
    /// let trc = Trc::new_cyclic(|x| T(x.clone()));
    /// ```
    #[inline]
    pub fn new_cyclic<F>(data_fn: F) -> Self
    where
        F: FnOnce(&Weak<T>) -> T,
    {
        let shareddata: NonNull<_> = Box::leak(Box::new(SharedTrcInternal {
            atomicref: AtomicUsize::new(0),
            weakcount: AtomicUsize::new(1),
            data: core::mem::MaybeUninit::<T>::uninit(),
        }))
        .into();

        let init_ptr: NonNull<SharedTrcInternal<T>> = shareddata.cast();

        let weak: Weak<T> = Weak { data: init_ptr };
        let data = data_fn(&weak);
        core::mem::forget(weak);

        unsafe {
            let ptr = init_ptr.as_ptr();
            core::ptr::write(core::ptr::addr_of_mut!((*ptr).data), data);
            let prev = sum_value(
                &init_ptr.as_ref().atomicref,
                1,
                core::sync::atomic::Ordering::AcqRel,
            );
            if prev > MAX_REFCOUNT {
                panic!("Overflow of maximum strong reference count.");
            }
        }

        let tbx = Box::new(1);

        Trc {
            threadref: NonNull::from(Box::leak(tbx)),
            shared: init_ptr,
        }
    }

    /// Creates a new `Pin<Trc<T>>`. If `T` does not implement [`Unpin`], then the data will be pinned in memory and unable to be moved.
    #[inline]
    pub fn pin(data: T) -> Pin<Trc<T>> {
        unsafe { Pin::new_unchecked(Trc::new(data)) }
    }

    /// Returns the inner value if the `Trc` has exactly one atomic and local reference.
    /// Otherwise, an [`Err`] is returned with the same `Trc` that was passed in.
    /// This will succeed even if there are outstanding weak references.
    ///
    /// # Examples
    /// This works:
    /// ```
    /// use trc::Trc;
    /// use std::ops::DerefMut;
    ///
    /// let mut trc = Trc::new(100);
    /// let inner = Trc::try_unwrap(trc).ok();
    /// ```
    ///
    /// This does not work:
    /// ```
    /// use trc::Trc;
    /// use std::ops::DerefMut;
    ///
    /// let mut trc = Trc::new(100);
    /// let _ = trc.clone();
    /// let inner = Trc::try_unwrap(trc).ok();
    /// ```
    #[inline]
    pub fn try_unwrap(mut this: Self) -> Result<T, Self> {
        if unsafe { this.shared.as_ref() }
            .atomicref
            .load(core::sync::atomic::Ordering::Acquire)
            != 1
            || *unsafe { this.threadref.as_ref() } != 1
        {
            return Err(this);
        }
        *unsafe { this.threadref.as_mut() } -= 1;

        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);

        unsafe {
            let elem = ptr::read(&this.shared.as_ref().data);
            drop(Box::from_raw(this.threadref.as_ptr()));

            //Clean up implicit self-reference
            drop(Weak { data: this.shared });
            core::mem::forget(this);

            Ok(elem)
        }
    }

    /// Returns the inner value if the `Trc` has exactly one atomic and local reference.
    /// Otherwise, a [`None`] is returned and the `Trc` is dropped.
    /// This will succeed even if there are outstanding weak references.
    /// If `into_inner` is called on every clone of `Trc`, it is guaranteed that exactly one will return the inner value `T`.
    /// This means the inner value is not dropped. The similar expression `Trc::try_unwrap(this).ok` does not offer such a guarantee.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let x = Trc::new(3i32);
    /// let y = Trc::clone(&x);
    ///
    /// let shared_x: SharedTrc<i32> = x.into();
    /// let shared_y: SharedTrc<i32> = y.into();
    ///
    /// // Two threads calling `Trc::into_inner` on both clones of an `Trc`:
    /// let x_thread = std::thread::spawn(|| Trc::into_inner(SharedTrc::to_trc(shared_x)));
    /// let y_thread = std::thread::spawn(|| Trc::into_inner(SharedTrc::to_trc(shared_y)));
    ///
    /// let x_inner_value = x_thread.join().unwrap();
    /// let y_inner_value = y_thread.join().unwrap();
    ///
    /// // One of the threads is guaranteed to receive the inner value:
    /// assert!(matches!(
    ///     (x_inner_value, y_inner_value),
    ///     (None, Some(3)) | (Some(3), None)
    /// ));
    /// ```
    ///
    #[inline]
    pub fn into_inner(this: Self) -> Option<T> {
        let this = core::mem::ManuallyDrop::new(this);

        if sub_value(
            &unsafe { this.shared.as_ref() }.atomicref,
            1,
            core::sync::atomic::Ordering::Release,
        ) != 1
            || *unsafe { this.threadref.as_ref() } != 1
        {
            drop(unsafe { Box::from_raw(this.threadref.as_ptr()) });
            return None;
        }

        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);

        let elem = unsafe { core::ptr::read(addr_of_mut!((*this.shared.as_ptr()).data)) };
        drop(unsafe { Box::from_raw(this.threadref.as_ptr()) });

        //Clean up implicit self-reference
        drop(Weak { data: this.shared });

        Some(elem)
    }

    /// Converts a `*const T` into `Trc<T>`. The caller must uphold the below safety constraints.
    /// To avoid a memory leak, be sure to call `from_raw` to reclaim the allocation.
    ///
    /// # Safety
    /// - The given pointer must be a valid pointer to `T` that came from `into_raw`.
    /// - After `from_raw`, the pointer must not be accessed.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let trc = Trc::new(100);
    /// let ptr = Trc::into_raw(trc);
    ///
    /// assert_eq!(unsafe { *ptr }, 100);
    ///
    /// unsafe { Trc::from_raw(ptr) };
    /// ```
    pub unsafe fn from_raw(ptr: *const T) -> Self {
        let tbx = Box::new(1);

        let layout = Layout::new::<SharedTrcInternal<()>>();
        let n = layout.size();

        let data_ptr = (ptr as *const u8).sub(n) as *mut SharedTrcInternal<T>;

        Trc {
            threadref: NonNull::from(Box::leak(tbx)),
            shared: NonNull::new_unchecked(data_ptr),
        }
    }
}

impl<T> Trc<[T]> {
    /// Constructs a new Trc slice with uninitialized contents.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let mut trc = Trc::new_uninit();
    ///
    /// Trc::get_mut(&mut trc).unwrap().write(5);
    ///
    /// let five = unsafe { trc.assume_init() };
    ///
    /// assert_eq!(*five, 5);
    /// ```
    pub fn new_uninit_slice(len: usize) -> Trc<[MaybeUninit<T>]> {
        let value_layout = Layout::array::<T>(len).unwrap();
        let layout = Layout::new::<SharedTrcInternal<()>>()
            .extend(value_layout)
            .unwrap()
            .0
            .pad_to_align();

        let res = slice_from_raw_parts_mut(unsafe { alloc(layout) } as *mut T, len)
            as *mut SharedTrcInternal<[MaybeUninit<T>]>;
        unsafe { write(&mut (*res).atomicref, AtomicUsize::new(1)) };
        unsafe { write(&mut (*res).weakcount, AtomicUsize::new(1)) };

        let elems = unsafe { addr_of_mut!((*res).data) } as *mut MaybeUninit<T>;
        for i in 0..len {
            unsafe { write(elems.add(i), MaybeUninit::<T>::uninit()) };
        }
        let tbx = Box::new(1);

        Trc {
            threadref: NonNull::from(Box::leak(tbx)),
            shared: unsafe { NonNull::new_unchecked(res) },
        }
    }
}

impl<T> Trc<MaybeUninit<T>> {
    /// Converts to `Trc<T>`.
    ///
    /// # Safety
    /// As with `MaybeUninit::assume_init`, it is up to the caller to guarantee that the inner value really is in an initialized state.
    /// Calling this when the content is not yet fully initialized causes immediate undefined behavior.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let mut values = Trc::<[u32]>::new_uninit_slice(3);
    ///
    /// // Deferred initialization:
    /// let data = Trc::get_mut(&mut values).unwrap();
    /// data[0].write(1);
    /// data[1].write(2);
    /// data[2].write(3);
    ///
    /// let values = unsafe { values.assume_init() };
    ///
    /// assert_eq!(*values, [1, 2, 3])
    /// ```
    pub unsafe fn assume_init(self) -> Trc<T> {
        let threadref = self.threadref;
        Trc {
            shared: NonNull::new_unchecked(ManuallyDrop::new(self).shared.as_ptr().cast()),
            threadref,
        }
    }
}

impl<T> Trc<[MaybeUninit<T>]> {
    /// Converts to `Trc<[T]>`.
    ///
    /// # Safety
    /// As with `MaybeUninit::assume_init`, it is up to the caller to guarantee that the inner value really is in an initialized state.
    /// Calling this when the content is not yet fully initialized causes immediate undefined behavior.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let mut values = Trc::<[u32]>::new_uninit_slice(3);
    ///
    /// // Deferred initialization:
    /// let data = Trc::get_mut(&mut values).unwrap();
    /// data[0].write(1);
    /// data[1].write(2);
    /// data[2].write(3);
    ///
    /// let values = unsafe { values.assume_init() };
    ///
    /// assert_eq!(*values, [1, 2, 3])
    /// ```
    pub unsafe fn assume_init(self) -> Trc<[T]> {
        let threadref = self.threadref;
        Trc {
            shared: NonNull::new_unchecked(ManuallyDrop::new(self).shared.as_ptr() as _),
            threadref,
        }
    }
}

impl<T: ?Sized> Trc<T> {
    /// Return the local thread reference count of the object, which is how many `Trc<T>`s in this thread point to the data referenced by this `Trc<T>`.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let trc = Trc::new(100);
    /// assert!(Trc::local_count(&trc) == 1)
    /// ```
    #[inline]
    pub fn local_count(this: &Self) -> usize {
        *unsafe { this.threadref.as_ref() }
    }

    /// Return the atomic reference count of the object. This is how many threads are using the data referenced by this `Trc<T>`.
    /// ```
    /// use std::thread;
    /// use trc::Trc;
    /// use trc::SharedTrc;
    ///
    /// let trc = Trc::new(100);
    /// let shared = SharedTrc::from_trc(&trc);
    ///
    /// let handle = thread::spawn(move || {
    ///     let mut trc = SharedTrc::to_trc(shared);
    ///     assert_eq!(Trc::atomic_count(&trc), 2);
    /// });
    ///
    /// handle.join().unwrap();
    /// assert_eq!(Trc::atomic_count(&trc), 1);
    /// assert_eq!(*trc, 100);
    /// ```
    #[inline]
    pub fn atomic_count(this: &Self) -> usize {
        unsafe { this.shared.as_ref() }
            .atomicref
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    /// Return the weak count of the object. This is how many weak counts - across all threads - are pointing to the allocation inside of `Trc<T>`.
    /// It includes the implicit weak reference held by all `Trc<T>` to themselves.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::Weak;
    ///
    /// let trc = Trc::new(100i32);
    /// let weak = Trc::downgrade(&trc);
    /// let weak2 = Trc::downgrade(&trc);
    /// let new_trc = Weak::upgrade(&weak).expect("Value was dropped");
    /// drop(weak);
    /// assert_eq!(Trc::weak_count(&new_trc), 2);
    /// ```
    #[inline]
    pub fn weak_count(this: &Self) -> usize {
        unsafe { this.shared.as_ref() }
            .weakcount
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    /// Checks if the other `Trc<T>` is equal to this one according to their internal pointers.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let trc1 = Trc::new(100);
    /// let trc2 = trc1.clone();
    /// assert!(Trc::ptr_eq(&trc1, &trc2));
    /// ```
    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.shared.as_ptr() == other.shared.as_ptr()
    }

    /// Gets the raw pointer to the most inner layer of `Trc<T>`.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let trc = Trc::new(100);
    /// println!("{}", Trc::as_ptr(&trc) as usize)
    /// ```
    #[inline]
    pub fn as_ptr(this: &Self) -> *const T {
        let sharedptr = NonNull::as_ptr(this.shared);
        unsafe { addr_of_mut!((*sharedptr).data) }
    }

    /// Get a &mut reference to the internal data if there are no other `Trc` or [`Weak`] pointers to the same allocation.
    /// Otherwise, return [`None`] because it would be unsafe to mutate a shared value.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use std::ops::DerefMut;
    ///
    /// let mut trc = Trc::new(100);
    /// let mutref = Trc::get_mut(&mut trc).unwrap();
    /// *mutref = 300;
    /// assert_eq!(*trc, 300);
    /// ```
    #[inline]
    pub fn get_mut(this: &mut Self) -> Option<&mut T> {
        //Acquire the weakcount if it is == 1
        if unsafe { this.shared.as_ref() }
            .weakcount
            .compare_exchange(
                1,
                usize::MAX,
                core::sync::atomic::Ordering::Acquire,
                core::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
        {
            //Acquire the atomicref
            let unique = unsafe { this.shared.as_ref() }
                .atomicref
                .load(core::sync::atomic::Ordering::Acquire)
                == 1;

            //Synchronize with the previous Acquire
            unsafe { this.shared.as_ref() }
                .weakcount
                .store(1, core::sync::atomic::Ordering::Release);

            if unique && *unsafe { this.threadref.as_ref() } == 1 {
                Some(unsafe { &mut (*this.shared.as_ptr()).data })
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Converts a `Trc<T>` into `*const T`, without freeing the allocation.
    /// To avoid a memory leak, be sure to call `from_raw` to reclaim the allocation.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let trc = Trc::new(100);
    /// let ptr = Trc::into_raw(trc);
    ///
    /// assert_eq!(unsafe { *ptr }, 100);
    ///
    /// unsafe { Trc::from_raw(ptr) };
    /// ```
    pub fn into_raw(this: Self) -> *const T {
        let ptr = Self::as_ptr(&this);

        drop(unsafe { Box::from_raw(this.threadref.as_ptr()) });
        forget(this);
        ptr
    }
}

impl<T: Clone> Trc<T> {
    /// If we have the only strong and local reference to `T`, then unwrap it. Otherwise, clone `T` and return the clone.
    /// If `trc_t` is of type `Trc<T>`, this function is functionally equivalent to `(*trc_t).clone()`, but will avoid cloning the inner
    /// value where possible.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let inner = String::from("Trc");
    /// let ptr = inner.as_ptr();
    ///
    /// let trc = Trc::new(inner);
    /// let trc2 = trc.clone();
    /// let inner = Trc::unwrap_or_clone(trc);
    /// assert!(!std::ptr::eq(ptr, inner.as_ptr()));
    ///
    /// let inner = Trc::unwrap_or_clone(trc2);
    /// assert!(std::ptr::eq(ptr, inner.as_ptr()));
    /// ```
    #[inline]
    pub fn unwrap_or_clone(this: Self) -> T {
        Trc::try_unwrap(this).unwrap_or_else(|trc| (*trc).clone())
    }
}

impl<T: ?Sized> Trc<T> {
    /// Create a `Weak<T>` from a `Trc<T>`. This increments the weak count.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::Weak;
    ///
    /// let trc = Trc::new(100);
    /// let weak = Trc::downgrade(&trc);
    /// ```
    #[inline]
    pub fn downgrade(trc: &Trc<T>) -> Weak<T> {
        let prev = sum_value(
            &unsafe { trc.shared.as_ref() }.weakcount,
            1,
            core::sync::atomic::Ordering::Acquire,
        );
        if prev > MAX_REFCOUNT {
            panic!("Overflow of maximum weak reference count.");
        }
        Weak { data: trc.shared }
    }
}

impl<T: ?Sized> Deref for Trc<T> {
    type Target = T;

    /// Get an immutable reference to the internal data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use std::ops::Deref;
    ///
    /// let mut trc = Trc::new(100i32);
    /// assert_eq!(*trc, 100i32);
    /// assert_eq!(trc.deref(), &100i32);
    /// ```
    #[inline]
    fn deref(&self) -> &Self::Target {
        &unsafe { self.shared.as_ref() }.data
    }
}

impl<T: ?Sized> Drop for Trc<T> {
    #[inline]
    fn drop(&mut self) {
        *unsafe { self.threadref.as_mut() } -= 1;
        if *unsafe { self.threadref.as_ref() } == 0 {
            drop(unsafe { Box::from_raw(self.threadref.as_ptr()) });
            if sub_value(
                &unsafe { self.shared.as_ref() }.atomicref,
                1,
                core::sync::atomic::Ordering::Release,
            ) != 1
            {
                return;
            }

            core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
            unsafe { core::ptr::drop_in_place(addr_of_mut!((*self.shared.as_ptr()).data)) };
            Weak { data: self.shared };
        }
    }
}

impl<T: ?Sized> Clone for Trc<T> {
    /// Clone a `Trc<T>` (increment it's local reference count).
    /// It will panic if the local reference count overflows.
    /// ```
    /// use trc::Trc;
    ///
    /// let trc = Trc::new(100);
    /// let trc2 = trc.clone();
    /// assert_eq!(Trc::local_count(&trc), Trc::local_count(&trc2));
    /// ```
    #[inline(always)]
    fn clone(&self) -> Self {
        unsafe { *self.threadref.as_ptr() += 1 };
        if unsafe { *self.threadref.as_ptr() } > MAX_REFCOUNT {
            panic!("Overflow of maximum strong reference count.");
        }

        Trc {
            shared: self.shared,
            threadref: self.threadref,
        }
    }
}

impl<T: ?Sized> AsRef<T> for Trc<T> {
    fn as_ref(&self) -> &T {
        Trc::deref(self)
    }
}

impl<T: ?Sized> Borrow<T> for Trc<T> {
    fn borrow(&self) -> &T {
        self.as_ref()
    }
}

impl<T: ?Sized + Default> Default for Trc<T> {
    fn default() -> Self {
        Trc::new(Default::default())
    }
}

impl<T: Display> Display for Trc<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt((*self).deref(), f)
    }
}

impl<T: Debug> Debug for Trc<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt((*self).deref(), f)
    }
}

impl<T: ?Sized> Pointer for Trc<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Pointer::fmt(&addr_of!(unsafe { self.shared.as_ref() }.data), f)
    }
}

impl<T> From<T> for Trc<T> {
    /// Create a new `Trc<T>` from the provided data. This is equivalent to calling `Trc::new` on the same data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let trc = Trc::from(100);
    /// assert_eq!(*trc, 100);
    /// ```
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: Hash> Hash for Trc<T> {
    /// Pass the data contained in this `Trc<T>` to the provided hasher.
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.deref().hash(state);
    }
}

impl<T: PartialOrd> PartialOrd for Trc<T> {
    /// "Greater than or equal to" comparison for two `Trc<T>`s.
    ///
    /// Calls `.ge` on the data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let trc1 = Trc::from(100);
    /// let trc2 = Trc::from(100);
    /// assert!(trc1>=trc2);
    /// ```
    #[inline]
    fn ge(&self, other: &Self) -> bool {
        self.deref().ge(other.deref())
    }

    /// "Less than or equal to" comparison for two `Trc<T>`s.
    ///
    /// Calls `.le` on the data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let trc1 = Trc::from(100);
    /// let trc2 = Trc::from(100);
    /// assert!(trc1<=trc2);
    /// ```
    #[inline]
    fn le(&self, other: &Self) -> bool {
        self.deref().ge(other.deref())
    }

    /// "Greater than" comparison for two `Trc<T>`s.
    ///
    /// Calls `.gt` on the data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let trc1 = Trc::from(200);
    /// let trc2 = Trc::from(100);
    /// assert!(trc1>trc2);
    /// ```
    #[inline]
    fn gt(&self, other: &Self) -> bool {
        self.deref().gt(other.deref())
    }

    /// "Less than" comparison for two `Trc<T>`s.
    ///
    /// Calls `.lt` on the data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let trc1 = Trc::from(100);
    /// let trc2 = Trc::from(200);
    /// assert!(trc1<trc2);
    /// ```
    #[inline]
    fn lt(&self, other: &Self) -> bool {
        self.deref().lt(other.deref())
    }

    /// Partial comparison for two `Trc<T>`s.
    ///
    /// Calls `.partial_cmp` on the data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use std::cmp::Ordering;
    ///
    /// let trc1 = Trc::from(100);
    /// let trc2 = Trc::from(200);
    /// assert_eq!(Some(Ordering::Less), trc1.partial_cmp(&trc2));
    /// ```
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        self.deref().partial_cmp(other.deref())
    }
}

impl<T: Ord> Ord for Trc<T> {
    /// Comparison for two `Trc<T>`s. The two are compared by calling `.cmp` on the inner values.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use std::cmp::Ordering;
    ///
    /// let trc1 = Trc::from(100);
    /// let trc2 = Trc::from(200);
    /// assert_eq!(Ordering::Less, trc1.cmp(&trc2));
    /// ```
    #[inline]
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.deref().cmp(other.deref())
    }
}

impl<T: Eq> Eq for Trc<T> {}

impl<T: PartialEq> PartialEq for Trc<T> {
    /// Equality by value comparison for two `Trc<T>`s, even if the data is in different allocoations.
    ///
    /// Calls `.eq` on the data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let trc1 = Trc::from(100);
    /// let trc2 = Trc::from(100);
    /// assert!(trc1==trc2);
    /// ```
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.deref().eq(other.deref())
    }

    /// Inequality by value comparison for two `Trc<T>`s, even if the data is in different allocoations.
    ///
    /// Calls `.ne` on the data.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let trc1 = Trc::from(100);
    /// let trc2 = Trc::from(200);
    /// assert!(trc1!=trc2);
    /// ```
    #[allow(clippy::partialeq_ne_impl)]
    #[inline]
    fn ne(&self, other: &Self) -> bool {
        self.deref().ne(other.deref())
    }
}

impl<T: AsFd> AsFd for Trc<T> {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        (**self).as_fd()
    }
}

impl<T: AsRawFd> AsRawFd for Trc<T> {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        (**self).as_raw_fd()
    }
}

#[allow(deprecated)]
impl<T: Error> Error for Trc<T> {
    fn cause(&self) -> Option<&dyn Error> {
        (**self).cause()
    }
    fn description(&self) -> &str {
        (**self).description()
    }
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        (**self).source()
    }
}

impl<T: ?Sized> Unpin for Trc<T> {}
impl<T: ?Sized> UnwindSafe for Trc<T> {}

fn create_from_iterator_exact<T>(
    iterator: impl Iterator<Item = T> + ExactSizeIterator,
) -> *mut SharedTrcInternal<[T]> {
    let value_layout = Layout::array::<T>(iterator.len()).unwrap();
    let layout = Layout::new::<SharedTrcInternal<()>>()
        .extend(value_layout)
        .unwrap()
        .0
        .pad_to_align();

    let res = slice_from_raw_parts_mut(unsafe { alloc(layout) } as *mut T, iterator.len())
        as *mut SharedTrcInternal<[T]>;
    unsafe { write(&mut (*res).atomicref, AtomicUsize::new(1)) };
    unsafe { write(&mut (*res).weakcount, AtomicUsize::new(1)) };

    let elems = unsafe { addr_of_mut!((*res).data) } as *mut T;
    for (n, i) in iterator.enumerate() {
        unsafe { write(elems.add(n), i) };
    }
    res
}

trait TrcFromIter<T> {
    fn from_iter(slice: impl Iterator<Item = T> + ExactSizeIterator) -> Self;
}

impl<T: Clone + ?Sized> TrcFromIter<T> for Trc<[T]> {
    fn from_iter(slice: impl Iterator<Item = T> + ExactSizeIterator) -> Self {
        let shared = create_from_iterator_exact(slice);
        let tbx = Box::new(1);

        Trc {
            threadref: NonNull::from(Box::leak(tbx)),
            shared: unsafe { NonNull::new_unchecked(shared) },
        }
    }
}

impl<T: Clone + ?Sized> From<&[T]> for Trc<[T]> {
    /// From conversion from a reference to a slice of type `T` (`&[T]`) to a `Trc<[T]>`.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let vec = (1..100).collect::<Vec<i32>>();
    /// let slice = &vec[2..5];
    /// let trc = Trc::<[i32]>::from(slice);
    /// assert_eq!(&*trc, slice);
    /// ```
    fn from(value: &[T]) -> Trc<[T]> {
        <Self as TrcFromIter<T>>::from_iter(value.iter().cloned())
    }
}

impl<T: Clone + ?Sized> FromIterator<T> for Trc<[T]> {
    /// From conversion from an iterator (`impl IntoIterator<Item = T>`) to `Trc<[T]>`. Due to Rust's unstable trait specialization feature,
    /// there is no special case for iterators that implement [`ExactSizeIterator`].
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    ///
    /// let trc = Trc::<[i32]>::from_iter(vec![1,2,3]);
    /// assert_eq!(&*trc, vec![1,2,3]);
    /// ```
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::from(&iter.into_iter().collect::<Vec<_>>()[..])
    }
}

//TODO: Integration with standard library for both, or use lib & conditional for just CoerceUnsized
//impl<T: ?Sized + std::marker::Unsize<U>, U: ?Sized> std::ops::CoerceUnsized<Trc<U>> for Trc<T> {}
//impl<T: ?Sized> std::ops::Receiver for Trc<T> {}



impl<T: ?Sized> Drop for Weak<T> {
    #[inline]
    fn drop(&mut self) {
        if sub_value(
            unsafe { &(*self.data.as_ptr()).weakcount },
            1,
            core::sync::atomic::Ordering::Release,
        ) != 1
        {
            return;
        }

        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);

        let layout = Layout::for_value(unsafe { &*self.data.as_ptr() });
        unsafe { std::alloc::dealloc(self.data.as_ptr().cast(), layout) };
    }
}

impl<T: ?Sized> Weak<T> {
    /// Create a `Trc<T>` from a `Weak<T>`. Because `Weak<T>` does not own the value, it might have been dropped already. If it has, a `None` is returned.
    /// If the value has not been dropped, then this function a) decrements the weak count, and b) increments the atomic reference count of the object.
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::Weak;
    ///
    /// let trc = Trc::new(100i32);
    /// let weak = Trc::downgrade(&trc);
    /// let new_trc = Weak::upgrade(&weak).expect("Value was dropped");
    /// drop(weak);
    /// assert_eq!(*new_trc, 100i32);
    /// ```
    #[inline]
    pub fn upgrade(this: &Self) -> Option<Trc<T>> {
        unsafe { this.data.as_ref() }
            .atomicref
            .fetch_update(
                core::sync::atomic::Ordering::Acquire,
                core::sync::atomic::Ordering::Relaxed,
                |n| {
                    // Any write of 0 we can observe leaves the field in permanently zero state.
                    if n == 0 {
                        return None;
                    }
                    // See comments in `Arc::clone` for why we do this (for `mem::forget`).
                    assert!(
                        n <= MAX_REFCOUNT,
                        "Overflow of maximum strong reference count."
                    );
                    Some(n + 1)
                },
            )
            .ok()
            .map(|_| {
                let tbx = Box::new(1);
                Trc {
                    threadref: NonNull::from(Box::leak(tbx)),
                    shared: this.data,
                }
            })
    }
}

impl<T: ?Sized> Clone for Weak<T> {
    /// Clone a `Weak<T>` (increment the weak count).
    ///
    /// # Examples
    /// ```
    /// use trc::Trc;
    /// use trc::Weak;
    ///
    /// let trc = Trc::new(100);
    /// let weak1 = Trc::downgrade(&trc);
    /// let weak2 = weak1.clone();
    /// assert_eq!(Trc::weak_count(&trc), 3);
    /// ```
    #[inline]
    fn clone(&self) -> Self {
        let prev = sum_value(
            &unsafe { self.data.as_ref() }.weakcount,
            1,
            core::sync::atomic::Ordering::Relaxed,
        );

        //If an absurd number of threads are created, and then they are aborted before this, UB can
        //occur if the refcount wraps around.
        if prev > MAX_REFCOUNT {
            panic!("Overflow of maximum weak reference count.");
        }

        Weak { data: self.data }
    }
}

unsafe impl<T: Sync + Send> Send for Weak<T> {}
unsafe impl<T: Sync + Send> Sync for Weak<T> {}
