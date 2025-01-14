use crate::ebr::{Arc, AtomicArc, Barrier, Ptr, Tag};
use crate::wait_queue::{AsyncWait, WaitQueue};

use std::borrow::Borrow;
use std::mem::{needs_drop, MaybeUninit};
use std::ptr;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{fence, AtomicU32};

/// [`DataBlock`] is a type alias of a raw memory chunk that may contain entry instances.
pub type DataBlock<K, V, const LEN: usize> = [MaybeUninit<(K, V)>; LEN];

/// The size of a [`Bucket`].
pub const BUCKET_LEN: usize = 32;

/// The size of the linked data block.
const LINKED_LEN: usize = BUCKET_LEN / 4;

/// State bits.
const KILLED: u32 = 1_u32 << 31;
const WAITING: u32 = 1_u32 << 30;
const LOCK: u32 = 1_u32 << 29;
const SLOCK_MAX: u32 = LOCK - 1;
const LOCK_MASK: u32 = LOCK | SLOCK_MAX;

/// [`Bucket`] is a small fixed-size hash table with linear probing.
pub(crate) struct Bucket<K: Eq, V, const LOCK_FREE: bool> {
    /// The state of the [`Bucket`].
    state: AtomicU32,

    /// The number of valid entries in the [`Bucket`].
    num_entries: u32,

    /// The metadata of the [`Bucket`].
    metadata: Metadata<K, V, BUCKET_LEN>,

    /// The wait queue of the [`Bucket`].
    wait_queue: WaitQueue,
}

impl<K: Eq, V, const LOCK_FREE: bool> Default for Bucket<K, V, LOCK_FREE> {
    fn default() -> Self {
        Bucket::<K, V, LOCK_FREE> {
            state: AtomicU32::new(0),
            num_entries: 0,
            metadata: Metadata::default(),
            wait_queue: WaitQueue::default(),
        }
    }
}

impl<K: Eq, V, const LOCK_FREE: bool> Bucket<K, V, LOCK_FREE> {
    /// Returns true if the [`Bucket`] has been killed.
    #[inline]
    pub(crate) fn killed(&self) -> bool {
        (self.state.load(Relaxed) & KILLED) == KILLED
    }

    /// Returns the number of entries in the [`Bucket`].
    #[inline]
    pub(crate) fn num_entries(&self) -> usize {
        self.num_entries as usize
    }

    /// Returns `true` if the [`Bucket`] needs to be rebuilt.
    ///
    /// If `LOCK_FREE == true`, removed entries are not dropped occupying the slots, therefore
    /// rebuilding the [`Bucket`] might be needed to keep the [`Bucket`] as small as possible.
    #[inline]
    pub(crate) fn need_rebuild(&self) -> bool {
        self.metadata.removed_bitmap == (u32::MAX >> (32 - BUCKET_LEN))
    }

    /// Searches for an entry associated with the given key.
    #[inline]
    pub(crate) fn search<'b, Q>(
        &'b self,
        data_block: &'b DataBlock<K, V, BUCKET_LEN>,
        key: &Q,
        partial_hash: u8,
        barrier: &'b Barrier,
    ) -> Option<&'b (K, V)>
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        if self.num_entries == 0 {
            return None;
        }

        if let Some((_, entry_ref)) =
            Self::search_entry(&self.metadata, data_block, key, partial_hash)
        {
            return Some(entry_ref);
        }

        let mut link_ptr = self.metadata.link.load(Acquire, barrier);
        while let Some(link) = link_ptr.as_ref() {
            if let Some((_, entry_ref)) =
                Self::search_entry(&link.metadata, &link.data_block, key, partial_hash)
            {
                return Some(entry_ref);
            }
            link_ptr = link.metadata.link.load(Acquire, barrier);
        }

        None
    }

    /// Drops entries in the given [`DataBlock`].
    ///
    /// The [`Bucket`] and the [`DataBlock`] should never be used afterwards.
    #[inline]
    pub(crate) unsafe fn drop_entries(
        &mut self,
        data_block: &DataBlock<K, V, BUCKET_LEN>,
        barrier: &Barrier,
    ) {
        if !self.metadata.link.load(Relaxed, barrier).is_null() {
            self.clear_links(barrier);
        }
        if needs_drop::<(K, V)>() && self.metadata.occupied_bitmap != 0 {
            let mut index = self.metadata.occupied_bitmap.trailing_zeros();
            while index != 32 {
                ptr::drop_in_place(data_block[index as usize].as_ptr() as *mut (K, V));
                self.metadata.occupied_bitmap -= 1_u32 << index;
                index = self.metadata.occupied_bitmap.trailing_zeros();
            }
        }
    }

    /// Gets an [`EntryPtr`] pointing to an entry associated with the given key.
    #[inline]
    fn get<'b, Q>(
        &self,
        data_block: &'b DataBlock<K, V, BUCKET_LEN>,
        key: &Q,
        partial_hash: u8,
        barrier: &'b Barrier,
    ) -> EntryPtr<'b, K, V, LOCK_FREE>
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        if self.num_entries == 0 {
            return EntryPtr::new(barrier);
        }

        if let Some((index, _)) = Self::search_entry(&self.metadata, data_block, key, partial_hash)
        {
            return EntryPtr {
                current_link_ptr: Ptr::null(),
                current_index: index,
            };
        }

        let mut current_link_ptr = self.metadata.link.load(Acquire, barrier);
        while let Some(link) = current_link_ptr.as_ref() {
            if let Some((index, _)) =
                Self::search_entry(&link.metadata, &link.data_block, key, partial_hash)
            {
                return EntryPtr {
                    current_link_ptr,
                    current_index: index,
                };
            }
            current_link_ptr = link.metadata.link.load(Acquire, barrier);
        }

        EntryPtr::new(barrier)
    }

    /// Searches the given data block for an entry matching the key.
    #[allow(clippy::cast_possible_truncation)]
    fn search_entry<'b, Q, const LEN: usize>(
        metadata: &'b Metadata<K, V, LEN>,
        data_block: &'b DataBlock<K, V, LEN>,
        key: &Q,
        partial_hash: u8,
    ) -> Option<(usize, &'b (K, V))>
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        let mut bitmap = if LOCK_FREE {
            metadata.occupied_bitmap & (!metadata.removed_bitmap)
        } else {
            metadata.occupied_bitmap
        };
        if LOCK_FREE {
            fence(Acquire);
        }

        let preferred_index = usize::from(partial_hash) % LEN;
        if (bitmap & (1_u32 << preferred_index)) != 0
            && metadata.partial_hash_array[preferred_index] == partial_hash
        {
            let entry_ref = unsafe { &(*data_block[preferred_index].as_ptr()) };
            if entry_ref.0.borrow() == key {
                return Some((preferred_index, entry_ref));
            }
        }

        bitmap = if LEN == BUCKET_LEN {
            debug_assert_eq!(LEN, 32);
            bitmap.rotate_right(preferred_index as u32) & (!1_u32)
        } else {
            debug_assert_eq!(LEN, 8);
            u32::from((bitmap as u8).rotate_right(preferred_index as u32) & (!1_u8))
        };
        let mut offset = bitmap.trailing_zeros();
        while offset != 32 {
            let index = (preferred_index + offset as usize) % LEN;
            if metadata.partial_hash_array[index] == partial_hash {
                let entry_ref = unsafe { &(*data_block[index].as_ptr()) };
                if entry_ref.0.borrow() == key {
                    return Some((index, entry_ref));
                }
            }
            bitmap -= 1_u32 << offset;
            offset = bitmap.trailing_zeros();
        }

        None
    }

    /// Searches for a next closest valid entry slot number from the current one in the bitmap.
    ///
    /// If the specified slot is valid, it returns the specified one.
    fn next_entry<Q, const LEN: usize>(
        metadata: &Metadata<K, V, LEN>,
        current_index: usize,
    ) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        if current_index >= LEN {
            return None;
        }

        let bitmap = if LOCK_FREE {
            (metadata.occupied_bitmap & (!metadata.removed_bitmap))
                & (!((1_u32 << current_index) - 1))
        } else {
            metadata.occupied_bitmap & (!((1_u32 << current_index) - 1))
        };

        if LOCK_FREE {
            fence(Acquire);
        }

        let next_index = bitmap.trailing_zeros() as usize;
        if next_index < LEN {
            return Some(next_index);
        }

        None
    }

    /// Clears all the linked arrays iteratively.
    fn clear_links(&mut self, barrier: &Barrier) {
        if let (Some(mut next), _) = self.metadata.link.swap((None, Tag::None), Acquire) {
            loop {
                let next_next = next.metadata.link.swap((None, Tag::None), Acquire);
                let released = if LOCK_FREE {
                    next.release(barrier)
                } else {
                    // The `LinkedBucket` should be dropped immediately.
                    unsafe { next.release_drop_in_place() }
                };
                debug_assert!(released);
                if let (Some(next_next), _) = next_next {
                    next = next_next;
                } else {
                    break;
                }
            }
        }
    }
}

pub struct EntryPtr<'b, K: Eq, V, const LOCK_FREE: bool> {
    current_link_ptr: Ptr<'b, LinkedBucket<K, V, LINKED_LEN>>,
    current_index: usize,
}

impl<'b, K: Eq, V, const LOCK_FREE: bool> EntryPtr<'b, K, V, LOCK_FREE> {
    /// Creates a new [`EntryPtr`].
    #[inline]
    pub(crate) fn new(_barrier: &'b Barrier) -> EntryPtr<'b, K, V, LOCK_FREE> {
        EntryPtr {
            current_link_ptr: Ptr::null(),
            current_index: BUCKET_LEN,
        }
    }

    /// Returns `true` if the [`EntryPtr`] points to a valid entry or fused.
    #[inline]
    pub(crate) fn is_valid(&self) -> bool {
        self.current_index != BUCKET_LEN
    }

    /// Moves the [`EntryPtr`] to point to the next valid entry.
    ///
    /// Returns `true` if it successfully found the next valid entry.
    #[inline]
    pub(crate) fn next(&mut self, bucket: &Bucket<K, V, LOCK_FREE>, barrier: &'b Barrier) -> bool {
        if self.current_index != usize::MAX {
            if self.current_link_ptr.is_null() && self.next_entry(&bucket.metadata, barrier) {
                return true;
            }
            while let Some(link) = self.current_link_ptr.as_ref() {
                if self.next_entry(&link.metadata, barrier) {
                    return true;
                }
            }

            // Fuse itself.
            self.current_index = usize::MAX;
        }

        false
    }

    /// Gets a reference to the entry.
    ///
    /// The [`EntryPtr`] must point to a valid entry.
    #[inline]
    pub(crate) fn get(&self, data_block: &'b DataBlock<K, V, BUCKET_LEN>) -> &'b (K, V) {
        debug_assert_ne!(self.current_index, usize::MAX);
        let entry_ptr = if let Some(link) = self.current_link_ptr.as_ref() {
            link.data_block[self.current_index].as_ptr()
        } else {
            data_block[self.current_index].as_ptr()
        };
        unsafe { &(*entry_ptr) }
    }

    /// Gets a mutable reference to the entry.
    ///
    /// The [`EntryPtr`] must point to a valid entry, and the associated [`Bucket`] must be locked.
    #[allow(clippy::mut_from_ref)]
    #[inline]
    pub(crate) fn get_mut(
        &mut self,
        data_block: &'b DataBlock<K, V, BUCKET_LEN>,
        _locker: &mut Locker<K, V, LOCK_FREE>,
    ) -> &'b mut (K, V) {
        debug_assert_ne!(self.current_index, usize::MAX);
        let entry_ptr = if let Some(link) = self.current_link_ptr.as_ref() {
            link.data_block[self.current_index].as_ptr() as *mut (K, V)
        } else {
            data_block[self.current_index].as_ptr() as *mut (K, V)
        };
        unsafe { &mut (*entry_ptr) }
    }

    /// Gets the partial hash value of the entry.
    ///
    /// The [`EntryPtr`] must point to a valid entry.
    #[inline]
    pub(crate) fn partial_hash(&self, bucket: &Bucket<K, V, LOCK_FREE>) -> u8 {
        debug_assert_ne!(self.current_index, usize::MAX);
        if let Some(link) = self.current_link_ptr.as_ref() {
            link.metadata.partial_hash_array[self.current_index]
        } else {
            bucket.metadata.partial_hash_array[self.current_index]
        }
    }

    /// Tries to remove the [`LinkedBucket`] from the linked list.
    ///
    /// It should only be invoked when the caller is holding a [`Locker`] on the [`Bucket`].
    fn unlink(
        &mut self,
        locker: &Locker<K, V, LOCK_FREE>,
        link: &LinkedBucket<K, V, LINKED_LEN>,
        barrier: &'b Barrier,
    ) {
        let prev_link_ptr = link.prev_link.load(Relaxed);
        let next_link = if LOCK_FREE {
            link.metadata.link.get_arc(Relaxed, barrier)
        } else {
            link.metadata.link.swap((None, Tag::None), Relaxed).0
        };
        if let Some(next_link) = next_link.as_ref() {
            next_link.prev_link.store(prev_link_ptr, Relaxed);
        }

        self.current_link_ptr = next_link
            .as_ref()
            .map_or_else(Ptr::null, |n| n.ptr(barrier));
        let old_link = if let Some(prev_link) = unsafe { prev_link_ptr.as_ref() } {
            prev_link
                .metadata
                .link
                .swap((next_link, Tag::None), Relaxed)
                .0
        } else {
            locker
                .bucket
                .metadata
                .link
                .swap((next_link, Tag::None), Relaxed)
                .0
        };
        let released = old_link.map_or(true, |l| {
            if LOCK_FREE {
                l.release(barrier)
            } else {
                // The `LinkedBucket` should be dropped immediately.
                unsafe { l.release_drop_in_place() }
            }
        });
        debug_assert!(released);

        if self.current_link_ptr.is_null() {
            // Fuse the pointer.
            self.current_index = usize::MAX;
        } else {
            // Go to the next `Link`.
            self.current_index = LINKED_LEN;
        }
    }

    /// Moves the [`EntryPtr`] to the next valid entry in the [`Bucket`].
    fn next_entry<const LEN: usize>(
        &mut self,
        metadata: &Metadata<K, V, LEN>,
        barrier: &'b Barrier,
    ) -> bool {
        // Search for the next valid entry.
        let current_index = if self.current_index == LEN {
            0
        } else {
            self.current_index + 1
        };
        if let Some(index) = Bucket::<K, V, LOCK_FREE>::next_entry(metadata, current_index) {
            self.current_index = index;
            return true;
        }

        self.current_link_ptr = metadata.link.load(Acquire, barrier);
        self.current_index = LINKED_LEN;

        false
    }
}

pub struct Locker<'b, K: Eq, V, const LOCK_FREE: bool> {
    bucket: &'b mut Bucket<K, V, LOCK_FREE>,
}

impl<'b, K: Eq, V, const LOCK_FREE: bool> Locker<'b, K, V, LOCK_FREE> {
    /// Locks the [`Bucket`].
    #[inline]
    pub(crate) fn lock(
        bucket: &'b mut Bucket<K, V, LOCK_FREE>,
        barrier: &'b Barrier,
    ) -> Option<Locker<'b, K, V, LOCK_FREE>> {
        let bucket_ptr = bucket as *mut Bucket<K, V, LOCK_FREE>;
        loop {
            if let Ok(locker) = Self::try_lock(unsafe { &mut *bucket_ptr }, barrier) {
                return locker;
            }
            if let Ok(locker) = unsafe { &*bucket_ptr }.wait_queue.wait_sync(|| {
                // Mark that there is a waiting thread.
                bucket.state.fetch_or(WAITING, Release);
                Self::try_lock(unsafe { &mut *bucket_ptr }, barrier)
            }) {
                return locker;
            }
        }
    }

    /// Tries to lock the [`Bucket`].
    #[inline]
    pub(crate) fn try_lock(
        bucket: &'b mut Bucket<K, V, LOCK_FREE>,
        _barrier: &'b Barrier,
    ) -> Result<Option<Locker<'b, K, V, LOCK_FREE>>, ()> {
        let current = bucket.state.load(Relaxed) & (!LOCK_MASK);
        if (current & KILLED) == KILLED {
            return Ok(None);
        }
        if bucket
            .state
            .compare_exchange(current, current | LOCK, Acquire, Relaxed)
            .is_ok()
        {
            Ok(Some(Locker { bucket }))
        } else {
            Err(())
        }
    }

    /// Tries to lock the [`Bucket`], and if it fails, pushes an [`AsyncWait`].
    #[inline]
    pub(crate) fn try_lock_or_wait(
        bucket: &'b mut Bucket<K, V, LOCK_FREE>,
        async_wait: &mut AsyncWait,
        barrier: &'b Barrier,
    ) -> Result<Option<Locker<'b, K, V, LOCK_FREE>>, ()> {
        let bucket_ptr = bucket as *mut Bucket<K, V, LOCK_FREE>;
        if let Ok(locker) = Self::try_lock(unsafe { &mut *bucket_ptr }, barrier) {
            return Ok(locker);
        }
        unsafe { &*bucket_ptr }
            .wait_queue
            .push_async_entry(async_wait, || {
                // Mark that there is a waiting thread.
                bucket.state.fetch_or(WAITING, Release);
                Self::try_lock(bucket, barrier)
            })
    }

    /// Returns a reference to the [`Bucket`].
    #[inline]
    pub(crate) fn bucket(&self) -> &Bucket<K, V, LOCK_FREE> {
        self.bucket
    }

    /// Gets an [`EntryPtr`] pointing to an entry associated with the given key.
    ///
    /// The returned [`EntryPtr`] points to a valid entry if the key is found.
    #[inline]
    pub(crate) fn get<Q>(
        &self,
        data_block: &'b DataBlock<K, V, BUCKET_LEN>,
        key: &Q,
        partial_hash: u8,
        barrier: &'b Barrier,
    ) -> EntryPtr<'b, K, V, LOCK_FREE>
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        self.bucket.get(data_block, key, partial_hash, barrier)
    }

    /// Reserves memory for insertion, and then constructs the key-value pair.
    ///
    /// `C` must not panic otherwise it leads to undefined behavior.
    #[inline]
    pub(crate) fn insert_with<C: FnOnce() -> (K, V)>(
        &mut self,
        data_block: &'b DataBlock<K, V, BUCKET_LEN>,
        partial_hash: u8,
        constructor: C,
        barrier: &'b Barrier,
    ) -> EntryPtr<'b, K, V, LOCK_FREE> {
        assert!(self.bucket.num_entries != u32::MAX, "array overflow");

        let free_index = Self::get_free_index::<BUCKET_LEN>(
            self.bucket.metadata.occupied_bitmap,
            partial_hash as usize % BUCKET_LEN,
        );
        if free_index == BUCKET_LEN {
            let preferred_index = partial_hash as usize % LINKED_LEN;
            let mut link_ptr = self.bucket.metadata.link.load(Acquire, barrier);
            while let Some(link_mut) =
                unsafe { (link_ptr.as_raw() as *mut LinkedBucket<K, V, LINKED_LEN>).as_mut() }
            {
                let free_index = Self::get_free_index::<LINKED_LEN>(
                    link_mut.metadata.occupied_bitmap,
                    preferred_index,
                );
                if free_index != LINKED_LEN {
                    Self::insert_entry_with(
                        &mut link_mut.metadata,
                        &link_mut.data_block,
                        free_index,
                        partial_hash,
                        constructor,
                    );
                    self.bucket.num_entries += 1;
                    return EntryPtr {
                        current_link_ptr: link_ptr,
                        current_index: free_index,
                    };
                }
                link_ptr = link_mut.metadata.link.load(Acquire, barrier);
            }

            // Insert a new `LinkedBucket` at the linked list head.
            let head = self.bucket.metadata.link.get_arc(Relaxed, barrier);
            let link = unsafe { Arc::new_unchecked(LinkedBucket::new(head)) };
            let link_ptr = link.ptr(barrier);
            unsafe {
                let link_mut = &mut *(link_ptr.as_raw() as *mut LinkedBucket<K, V, LINKED_LEN>);
                link_mut.data_block[preferred_index]
                    .as_mut_ptr()
                    .write(constructor());
                link_mut.metadata.partial_hash_array[preferred_index] = partial_hash;
                link_mut.metadata.occupied_bitmap |= 1_u32 << preferred_index;
            }
            if let Some(head) = link.metadata.link.load(Relaxed, barrier).as_ref() {
                head.prev_link.store(
                    link.as_ptr() as *mut LinkedBucket<K, V, LINKED_LEN>,
                    Relaxed,
                );
            }
            self.bucket
                .metadata
                .link
                .swap((Some(link), Tag::None), Release);
            self.bucket.num_entries += 1;
            EntryPtr {
                current_link_ptr: link_ptr,
                current_index: preferred_index,
            }
        } else {
            Self::insert_entry_with(
                &mut self.bucket.metadata,
                data_block,
                free_index,
                partial_hash,
                constructor,
            );
            self.bucket.num_entries += 1;
            EntryPtr {
                current_link_ptr: Ptr::null(),
                current_index: free_index,
            }
        }
    }

    /// Removes the key-value pair being pointed by the given [`EntryPtr`].
    #[inline]
    pub(crate) fn erase(
        &mut self,
        data_block: &DataBlock<K, V, BUCKET_LEN>,
        entry_ptr: &mut EntryPtr<K, V, LOCK_FREE>,
    ) -> Option<(K, V)> {
        debug_assert_ne!(entry_ptr.current_index, usize::MAX);

        self.bucket.num_entries -= 1;
        let link_ptr = entry_ptr.current_link_ptr.as_raw() as *mut LinkedBucket<K, V, LINKED_LEN>;
        if let Some(link_mut) = unsafe { link_ptr.as_mut() } {
            if LOCK_FREE {
                debug_assert_eq!(
                    link_mut.metadata.removed_bitmap & (1_u32 << entry_ptr.current_index),
                    0
                );
                link_mut.metadata.removed_bitmap |= 1_u32 << entry_ptr.current_index;
            } else {
                debug_assert_ne!(
                    link_mut.metadata.occupied_bitmap & (1_u32 << entry_ptr.current_index),
                    0
                );
                link_mut.metadata.occupied_bitmap &= !(1_u32 << entry_ptr.current_index);
                return Some(unsafe {
                    link_mut.data_block[entry_ptr.current_index].as_ptr().read()
                });
            }
        } else if LOCK_FREE {
            debug_assert_eq!(
                self.bucket.metadata.removed_bitmap & (1_u32 << entry_ptr.current_index),
                0
            );
            self.bucket.metadata.removed_bitmap |= 1_u32 << entry_ptr.current_index;
        } else {
            debug_assert_ne!(
                self.bucket.metadata.occupied_bitmap & (1_u32 << entry_ptr.current_index),
                0
            );
            self.bucket.metadata.occupied_bitmap &= !(1_u32 << entry_ptr.current_index);
            return Some(unsafe { data_block[entry_ptr.current_index].as_ptr().read() });
        }

        None
    }

    /// Extracts the key-value pair being pointed by the [`EntryPtr`].
    #[inline]
    pub(crate) fn extract<'e>(
        &mut self,
        data_block: &DataBlock<K, V, BUCKET_LEN>,
        entry_ptr: &mut EntryPtr<'e, K, V, LOCK_FREE>,
        barrier: &'e Barrier,
    ) -> (K, V) {
        debug_assert!(!LOCK_FREE);
        let link_ptr = entry_ptr.current_link_ptr.as_raw() as *mut LinkedBucket<K, V, LINKED_LEN>;
        if let Some(link_mut) = unsafe { link_ptr.as_mut() } {
            let extracted = Self::extract_entry(
                &mut link_mut.metadata,
                &link_mut.data_block,
                entry_ptr.current_index,
                &mut self.bucket.num_entries,
            );
            if link_mut.metadata.occupied_bitmap == 0 {
                entry_ptr.unlink(self, link_mut, barrier);
            }
            extracted
        } else {
            Self::extract_entry(
                &mut self.bucket.metadata,
                data_block,
                entry_ptr.current_index,
                &mut self.bucket.num_entries,
            )
        }
    }

    /// Purges all the data.
    #[inline]
    pub(crate) fn purge(&mut self, barrier: &Barrier) {
        if LOCK_FREE {
            self.bucket.metadata.removed_bitmap = self.bucket.metadata.occupied_bitmap;
        }
        self.bucket.state.fetch_or(KILLED, Release);
        self.bucket.num_entries = 0;
        if !self.bucket.metadata.link.load(Relaxed, barrier).is_null() {
            self.bucket.clear_links(barrier);
        }
    }

    /// Gets the most optimal free slot index from the given bitmap.
    fn get_free_index<const LEN: usize>(bitmap: u32, preferred_index: usize) -> usize {
        let mut free_index = (bitmap | ((1_u32 << preferred_index) - 1)).trailing_ones() as usize;
        if free_index == LEN {
            free_index = bitmap.trailing_ones() as usize;
        }
        free_index
    }

    /// Inserts a key-value pair in the slot.
    fn insert_entry_with<C: FnOnce() -> (K, V), const LEN: usize>(
        metadata: &mut Metadata<K, V, LEN>,
        data_block: &DataBlock<K, V, LEN>,
        index: usize,
        partial_hash: u8,
        constructor: C,
    ) {
        debug_assert!(index < LEN);

        unsafe {
            (data_block[index].as_ptr() as *mut (K, V)).write(constructor());
            metadata.partial_hash_array[index] = partial_hash;
            if LOCK_FREE {
                fence(Release);
            }
            metadata.occupied_bitmap |= 1_u32 << index;
        }
    }

    /// Extracts and removes the key-value pair in the slot.
    fn extract_entry<const LEN: usize>(
        metadata: &mut Metadata<K, V, LEN>,
        data_block: &DataBlock<K, V, LEN>,
        index: usize,
        num_entries_field: &mut u32,
    ) -> (K, V) {
        debug_assert!(index < LEN);

        *num_entries_field -= 1;
        metadata.occupied_bitmap &= !(1_u32 << index);
        let entry_ptr = data_block[index].as_ptr() as *mut (K, V);
        unsafe { ptr::read(entry_ptr) }
    }
}

impl<'b, K: Eq, V, const LOCK_FREE: bool> Drop for Locker<'b, K, V, LOCK_FREE> {
    #[inline]
    fn drop(&mut self) {
        let mut current = self.bucket.state.load(Relaxed);
        loop {
            let wakeup = (current & WAITING) == WAITING;
            match self.bucket.state.compare_exchange(
                current,
                current & (!(WAITING | LOCK)),
                Release,
                Relaxed,
            ) {
                Ok(_) => {
                    if wakeup {
                        self.bucket.wait_queue.signal();
                    }
                    break;
                }
                Err(result) => current = result,
            }
        }
    }
}

pub struct Reader<'b, K: Eq, V, const LOCK_FREE: bool> {
    bucket: &'b Bucket<K, V, LOCK_FREE>,
}

impl<'b, K: Eq, V, const LOCK_FREE: bool> Reader<'b, K, V, LOCK_FREE> {
    /// Locks the given [`Bucket`].
    #[inline]
    pub(crate) fn lock(
        bucket: &'b Bucket<K, V, LOCK_FREE>,
        barrier: &'b Barrier,
    ) -> Option<Reader<'b, K, V, LOCK_FREE>> {
        loop {
            if let Ok(reader) = Self::try_lock(bucket, barrier) {
                return reader;
            }
            if let Ok(reader) = bucket.wait_queue.wait_sync(|| {
                // Mark that there is a waiting thread.
                bucket.state.fetch_or(WAITING, Release);
                Self::try_lock(bucket, barrier)
            }) {
                return reader;
            }
        }
    }

    /// Tries to lock the [`Bucket`], and if it fails, pushes an [`AsyncWait`].
    #[inline]
    pub(crate) fn try_lock_or_wait(
        bucket: &'b Bucket<K, V, LOCK_FREE>,
        async_wait: &mut AsyncWait,
        barrier: &'b Barrier,
    ) -> Result<Option<Reader<'b, K, V, LOCK_FREE>>, ()> {
        if let Ok(reader) = Self::try_lock(bucket, barrier) {
            return Ok(reader);
        }
        bucket.wait_queue.push_async_entry(async_wait, || {
            // Mark that there is a waiting thread.
            bucket.state.fetch_or(WAITING, Release);
            Self::try_lock(bucket, barrier)
        })
    }

    /// Returns a reference to the [`Bucket`].
    #[inline]
    pub(crate) fn bucket(&self) -> &'b Bucket<K, V, LOCK_FREE> {
        self.bucket
    }

    /// Tries to lock the [`Bucket`].
    fn try_lock(
        bucket: &'b Bucket<K, V, LOCK_FREE>,
        _barrier: &'b Barrier,
    ) -> Result<Option<Reader<'b, K, V, LOCK_FREE>>, ()> {
        let current = bucket.state.load(Relaxed);
        if (current & LOCK_MASK) >= SLOCK_MAX {
            return Err(());
        }
        if (current & KILLED) >= KILLED {
            return Ok(None);
        }
        if bucket
            .state
            .compare_exchange(current, current + 1, Acquire, Relaxed)
            .is_ok()
        {
            Ok(Some(Reader { bucket }))
        } else {
            Err(())
        }
    }
}

impl<'b, K: Eq, V, const LOCK_FREE: bool> Drop for Reader<'b, K, V, LOCK_FREE> {
    #[inline]
    fn drop(&mut self) {
        let mut current = self.bucket.state.load(Relaxed);
        loop {
            let wakeup = (current & WAITING) == WAITING;
            let next = (current - 1) & !(WAITING);
            match self
                .bucket
                .state
                .compare_exchange(current, next, Relaxed, Relaxed)
            {
                Ok(_) => {
                    if wakeup {
                        self.bucket.wait_queue.signal();
                    }
                    break;
                }
                Err(result) => current = result,
            }
        }
    }
}

/// [`Metadata`] is a collection of metadata fields of [`Bucket`] and [`LinkedBucket`].
pub(crate) struct Metadata<K: Eq, V, const LEN: usize> {
    /// Linked list of entries.
    link: AtomicArc<LinkedBucket<K, V, LINKED_LEN>>,

    /// Bitmap for occupied slots.
    occupied_bitmap: u32,

    /// Bitmap for removed slots.
    removed_bitmap: u32,

    /// Partial hash array.
    partial_hash_array: [u8; LEN],
}

impl<K: Eq, V, const LEN: usize> Default for Metadata<K, V, LEN> {
    fn default() -> Self {
        Self {
            link: AtomicArc::default(),
            occupied_bitmap: 0,
            removed_bitmap: 0,
            partial_hash_array: [0; LEN],
        }
    }
}

/// [`LinkedBucket`] is a smaller [`Bucket`] that is attached to a [`Bucket`] as a linked list.
pub(crate) struct LinkedBucket<K: Eq, V, const LEN: usize> {
    metadata: Metadata<K, V, LEN>,
    data_block: DataBlock<K, V, LEN>,
    prev_link: AtomicPtr<LinkedBucket<K, V, LEN>>,
}

impl<K: Eq, V, const LEN: usize> LinkedBucket<K, V, LEN> {
    /// Creates an empty [`LinkedBucket`].
    fn new(next: Option<Arc<LinkedBucket<K, V, LINKED_LEN>>>) -> LinkedBucket<K, V, LEN> {
        LinkedBucket {
            metadata: Metadata {
                link: next.map_or_else(AtomicArc::null, AtomicArc::from),
                occupied_bitmap: 0,
                removed_bitmap: 0,
                partial_hash_array: [0; LEN],
            },
            data_block: unsafe { MaybeUninit::uninit().assume_init() },
            prev_link: AtomicPtr::default(),
        }
    }
}

impl<K: Eq, V, const LEN: usize> Drop for LinkedBucket<K, V, LEN> {
    fn drop(&mut self) {
        if needs_drop::<(K, V)>() {
            let mut index = self.metadata.occupied_bitmap.trailing_zeros();
            while index != 32 {
                unsafe {
                    ptr::drop_in_place(self.data_block[index as usize].as_mut_ptr());
                }
                self.metadata.occupied_bitmap -= 1_u32 << index;
                index = self.metadata.occupied_bitmap.trailing_zeros();
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::convert::TryInto;
    use std::sync::atomic::AtomicPtr;

    use tokio::sync;

    #[cfg_attr(miri, ignore)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 16)]
    #[cfg_attr(miri, ignore)]
    async fn queue() {
        let num_tasks = BUCKET_LEN + 2;
        let barrier = Arc::new(sync::Barrier::new(num_tasks));
        let data_block: Arc<DataBlock<usize, usize, BUCKET_LEN>> =
            Arc::new(unsafe { MaybeUninit::uninit().assume_init() });
        let mut bucket: Arc<Bucket<usize, usize, true>> = Arc::new(Bucket::default());
        let mut data: [u64; 128] = [0; 128];
        let mut task_handles = Vec::with_capacity(num_tasks);
        for task_id in 0..num_tasks {
            let barrier_copied = barrier.clone();
            let data_block_copied = data_block.clone();
            let bucket_copied = bucket.clone();
            let data_ptr = AtomicPtr::new(&mut data);
            task_handles.push(tokio::spawn(async move {
                barrier_copied.wait().await;
                let bucket_mut =
                    unsafe { &mut *(bucket_copied.as_ptr() as *mut Bucket<usize, usize, true>) };
                let barrier = Barrier::new();
                for i in 0..2048 {
                    let mut exclusive_locker = Locker::lock(bucket_mut, &barrier).unwrap();
                    let mut sum: u64 = 0;
                    for j in 0..128 {
                        unsafe {
                            sum += (*data_ptr.load(Relaxed))[j];
                            (*data_ptr.load(Relaxed))[j] = if i % 4 == 0 { 2 } else { 4 }
                        };
                    }
                    assert_eq!(sum % 256, 0);
                    if i == 0 {
                        exclusive_locker.insert_with(
                            &data_block_copied,
                            (task_id % BUCKET_LEN).try_into().unwrap(),
                            || (task_id, 0),
                            &barrier,
                        );
                    } else {
                        assert_eq!(
                            exclusive_locker
                                .bucket()
                                .search(
                                    &data_block_copied,
                                    &task_id,
                                    (task_id % BUCKET_LEN).try_into().unwrap(),
                                    &barrier
                                )
                                .unwrap(),
                            &(task_id, 0_usize)
                        );
                    }
                    drop(exclusive_locker);

                    let read_locker = Reader::lock(&*bucket_copied, &barrier).unwrap();
                    assert_eq!(
                        read_locker
                            .bucket()
                            .search(
                                &data_block_copied,
                                &task_id,
                                (task_id % BUCKET_LEN).try_into().unwrap(),
                                &barrier
                            )
                            .unwrap(),
                        &(task_id, 0_usize)
                    );
                }
            }));
        }
        for r in futures::future::join_all(task_handles).await {
            assert!(r.is_ok());
        }

        let sum: u64 = data.iter().sum();
        assert_eq!(sum % 256, 0);
        assert_eq!(bucket.num_entries(), num_tasks);

        let epoch_barrier = Barrier::new();
        for task_id in 0..num_tasks {
            assert_eq!(
                bucket.search(
                    &data_block,
                    &task_id,
                    (task_id % BUCKET_LEN).try_into().unwrap(),
                    &epoch_barrier
                ),
                Some(&(task_id, 0))
            );
        }

        let mut count = 0;
        let mut entry_ptr = EntryPtr::new(&epoch_barrier);
        while entry_ptr.next(&bucket, &epoch_barrier) {
            count += 1;
        }
        assert_eq!(bucket.num_entries(), count);

        let mut xlocker =
            Locker::lock(unsafe { bucket.get_mut().unwrap() }, &epoch_barrier).unwrap();
        xlocker.purge(&epoch_barrier);
        drop(xlocker);

        assert!(bucket.killed());
        assert_eq!(bucket.num_entries(), 0);
        assert!(Locker::lock(unsafe { bucket.get_mut().unwrap() }, &epoch_barrier).is_none());
    }
}
