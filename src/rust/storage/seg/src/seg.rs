// Copyright 2021 Twitter, Inc.
// Licensed under the Apache License, Version 2.0
// http://www.apache.org/licenses/LICENSE-2.0

//! Core datastructure

use crate::*;
use std::cmp::min;

use metrics::{static_metrics, Counter};

const RESERVE_RETRIES: usize = 3;

static_metrics! {
    static SEGMENT_REQUEST: Counter;
    static SEGMENT_REQUEST_FAILURE: Counter;
    static SEGMENT_REQUEST_SUCCESS: Counter;
}

/// A pre-allocated key-value store with eager expiration. It uses a
/// segment-structured design that stores data in fixed-size segments, grouping
/// objects with nearby expiration time into the same segment, and lifting most
/// per-object metadata into the shared segment header.
pub struct Seg {
    pub(crate) hashtable: HashTable,
    pub(crate) segments: Segments,
    pub(crate) ttl_buckets: TtlBuckets,
    // Used for testing: are the above structures restored?
    pub(crate) _restored: bool,
}

impl Seg {
    /// Returns a new `Builder` which is used to configure and construct a
    /// `Seg` instance.
    ///
    /// ```
    /// use seg::{Policy, Seg};
    ///
    /// const MB: usize = 1024 * 1024;
    ///
    /// // create a heap using 1MB segments
    /// let cache = Seg::builder()
    ///     .heap_size(64 * MB)
    ///     .segment_size(1 * MB as i32)
    ///     .hash_power(16)
    ///     .eviction(Policy::Random).build();
    /// ```
    pub fn builder() -> Builder {
        Builder::default()
    }

    // Returns a new `Demolisher` which is used to configure the graceful
    // deconstruction of a `Seg` instance.
    // 
    // Example code:
    // ```
    // let segment_size = 4096;
    // let segments = 64;
    // let heap_size = segments * segment_size as usize;
    // let datapool_path : Option<PathBuf> = Some(PathBuf::from(<path>));
    // let segments_fields_path: Option<PathBuf> = Some(PathBuf::from(<path>));
    // let ttl_buckets_path : Option<PathBuf> = Some(PathBuf::from(<path>));
    // let hashtable_path: Option<PathBuf> = Some(PathBuf::from(<path>));
    // 
    // // demolish cache by triggering graceful shutdown
    //     Seg::demolisher()
    //         .heap_size(heap_size)
    //         .datapool_path(datapool_path)
    //         .segments_fields_path(segments_fields_path) 
    //         .ttl_buckets_path(ttl_buckets_path)
    //         .hashtable_path(hashtable_path)
    //         .demolish(cache)
    // ```
    pub fn demolisher() -> Demolisher {
        Demolisher::default()
    }

    /// Gets a count of items in the `Seg` instance. This is an expensive
    /// operation and is only enabled for tests and builds with the `debug`
    /// feature enabled.
    ///
    /// ```
    /// use seg::{Policy, Seg};
    ///
    /// let mut cache = Seg::builder().build();
    /// assert_eq!(cache.items(), 0);
    /// ```
    #[cfg(any(test, feature = "debug"))]
    pub fn items(&mut self) -> usize {
        trace!("getting segment item counts");
        self.segments.items()
    }

    /// Get the item in the `Seg` with the provided key
    ///
    /// ```
    /// use seg::{Policy, Seg};
    /// use std::time::Duration;
    ///
    /// let mut cache = Seg::builder().build();
    /// assert!(cache.get(b"coffee").is_none());
    ///
    /// cache.insert(b"coffee", b"strong", None, Duration::ZERO);
    /// let item = cache.get(b"coffee").expect("didn't get item back");
    /// assert_eq!(item.value(), b"strong");
    /// ```
    pub fn get(&mut self, key: &[u8]) -> Option<Item> {
        self.hashtable.get(key, &mut self.segments)
    }

    /// Get the item in the `Seg` with the provided key without
    /// increasing the item frequency - useful for combined operations that
    /// check for presence - eg replace is a get + set
    /// ```
    /// use seg::{Policy, Seg};
    ///
    /// let mut cache = Seg::builder().build();
    /// assert!(cache.get_no_freq_incr(b"coffee").is_none());
    /// ```
    pub fn get_no_freq_incr(&mut self, key: &[u8]) -> Option<Item> {
        self.hashtable.get_no_freq_incr(key, &mut self.segments)
    }

    /// Insert a new item into the cache. May return an error indicating that
    /// the insert was not successful.
    /// ```
    /// use seg::{Policy, Seg};
    /// use std::time::Duration;
    ///
    /// let mut cache = Seg::builder().build();
    /// assert!(cache.get(b"drink").is_none());
    ///
    /// cache.insert(b"drink", b"coffee", None, Duration::ZERO);
    /// let item = cache.get(b"drink").expect("didn't get item back");
    /// assert_eq!(item.value(), b"coffee");
    ///
    /// cache.insert(b"drink", b"whisky", None, Duration::ZERO);
    /// let item = cache.get(b"drink").expect("didn't get item back");
    /// assert_eq!(item.value(), b"whisky");
    /// ```
    pub fn insert<'a>(
        &mut self,
        key: &'a [u8],
        value: &[u8],
        optional: Option<&[u8]>,
        ttl: std::time::Duration,
    ) -> Result<(), SegError<'a>> {
        // default optional data is empty
        let optional = optional.unwrap_or(&[]);

        // calculate size for item
        let size = (((ITEM_HDR_SIZE + key.len() + value.len() + optional.len()) >> 3) + 1) << 3;

        let ttl = Duration::from_secs(min(u32::MAX as u64, ttl.as_secs()) as u32);

        // try to get a `ReservedItem`
        let mut retries = RESERVE_RETRIES;
        let reserved;
        loop {
            // ccc: check tail segment of TTL bucket for free space.
            // ccc: If full, try to get a new segment from free q and make this the tail
            match self
                .ttl_buckets
                .get_mut_bucket(ttl)
                .reserve(size, &mut self.segments)  
            {
                Ok(mut reserved_item) => {
                    reserved_item.define(key, value, optional);
                    reserved = reserved_item;
                    break;
                }
                Err(TtlBucketsError::ItemOversized { size }) => {
                    return Err(SegError::ItemOversized { size, key });
                }
                Err(TtlBucketsError::NoFreeSegments) => {
                    if retries == RESERVE_RETRIES {
                        // first attempt to acquire a free segment, increment
                        // the stats
                        SEGMENT_REQUEST.increment();
                    }
                    if self
                        .segments
                        .evict(&mut self.ttl_buckets, &mut self.hashtable)
                        .is_err()
                    {
                        retries -= 1;
                    } else {
                        // we successfully got a segment, increment the stat and
                        // return to start of loop to reserve the item
                        SEGMENT_REQUEST_SUCCESS.increment();
                        continue;
                    }
                }
            }
            if retries == 0 {
                // segment acquire failed, increment the stat and return with
                // an error
                SEGMENT_REQUEST_FAILURE.increment();
                return Err(SegError::NoFreeSegments);
            }
            retries -= 1;
        }

        // insert into the hashtable, or roll-back by removing the item
        // TODO(bmartin): we can probably roll-back the offset and re-use the
        // space in the segment, currently we consume the space even if the
        // hashtable is overfull
        if self
            .hashtable
            .insert(
                reserved.item(),
                reserved.seg(),
                reserved.offset() as u64,
                &mut self.ttl_buckets,
                &mut self.segments,
            )
            .is_err()
        {
            let _ = self.segments.remove_at(
                reserved.seg(),
                reserved.offset(),
                false,
                &mut self.ttl_buckets,
                &mut self.hashtable,
            );
            Err(SegError::HashTableInsertEx)
        } else {
            Ok(())
        }
    }

    /// Performs a CAS operation, inserting the item only if the CAS value
    /// matches the current value for that item.
    ///
    /// ```
    /// use seg::{Policy, Seg, SegError};
    /// use std::time::Duration;
    ///
    /// let mut cache = Seg::builder().build();
    ///
    /// // If the item is not in the cache, CAS will fail as 'NotFound'
    /// assert_eq!(
    ///     cache.cas(b"drink", b"coffee", None, Duration::ZERO, 0),
    ///     Err(SegError::NotFound)
    /// );
    ///
    /// // If a stale CAS value is provided, CAS will fail as 'Exists'
    /// cache.insert(b"drink", b"coffee", None, Duration::ZERO);
    /// assert_eq!(
    ///     cache.cas(b"drink", b"coffee", None, Duration::ZERO, 0),
    ///     Err(SegError::Exists)
    /// );
    ///
    /// // Getting the CAS value and then performing the operation ensures
    /// // success in absence of a race with another client
    /// let current = cache.get(b"drink").expect("not found");
    /// assert!(cache.cas(b"drink", b"whisky", None, Duration::ZERO, current.cas()).is_ok());
    /// let item = cache.get(b"drink").expect("not found");
    /// assert_eq!(item.value(), b"whisky"); // item is updated
    /// ```
    pub fn cas<'a>(
        &mut self,
        key: &'a [u8],
        value: &[u8],
        optional: Option<&[u8]>,
        ttl: std::time::Duration,
        cas: u32,
    ) -> Result<(), SegError<'a>> {
        match self.hashtable.try_update_cas(key, cas, &mut self.segments) {
            Ok(()) => self.insert(key, value, optional, ttl),
            Err(e) => Err(e),
        }
    }

    /// Remove the item with the given key, returns a bool indicating if it was
    /// removed.
    /// ```
    /// use seg::{Policy, Seg, SegError};
    /// use std::time::Duration;
    ///
    /// let mut cache = Seg::builder().build();
    ///
    /// // If the item is not in the cache, delete will return false
    /// assert_eq!(cache.delete(b"coffee"), false);
    ///
    /// // And will return true on success
    /// cache.insert(b"coffee", b"strong", None, Duration::ZERO);
    /// assert!(cache.get(b"coffee").is_some());
    /// assert_eq!(cache.delete(b"coffee"), true);
    /// assert!(cache.get(b"coffee").is_none());
    /// ```
    // TODO(bmartin): a result would be better here
    pub fn delete(&mut self, key: &[u8]) -> bool {
        self.hashtable
            .delete(key, &mut self.ttl_buckets, &mut self.segments)
    }

    /// Loops through the TTL Buckets to handle eager expiration, returns the
    /// number of segments expired
    /// ```
    /// use seg::{Policy, Seg, SegError};
    /// use std::time::Duration;
    ///
    /// let mut cache = Seg::builder().build();
    ///
    /// // Insert an item with a short ttl
    /// cache.insert(b"coffee", b"strong", None, Duration::from_secs(5));
    ///
    /// // The item is still in the cache
    /// assert!(cache.get(b"coffee").is_some());
    ///
    /// // Delay and then trigger expiration
    /// std::thread::sleep(std::time::Duration::from_secs(6));
    /// cache.expire();
    ///
    /// // And the expired item is not in the cache
    /// assert!(cache.get(b"coffee").is_none());
    /// ```
    pub fn expire(&mut self) -> usize {
        common::time::refresh_clock();
        self.ttl_buckets
            .expire(&mut self.hashtable, &mut self.segments)
    }

    pub fn clear(&mut self) -> usize {
        common::time::refresh_clock();
        self.ttl_buckets
            .clear(&mut self.hashtable, &mut self.segments)
    }

    /// Checks the integrity of all segments
    /// *NOTE*: this operation is relatively expensive
    #[cfg(feature = "debug")]
    pub fn check_integrity(&mut self) -> Result<(), SegError> {
        if self.segments.check_integrity() {
            Ok(())
        } else {
            Err(SegError::DataCorrupted)
        }
    }

    // Used in testing to clone a `Seg` to compare with
    #[cfg(test)]
    pub(crate) fn clone(&self) -> Seg {
        let segments = self.segments.clone();
        let ttl_buckets = self.ttl_buckets.clone();
        let hashtable = self.hashtable.clone();
        Seg {
            segments,
            ttl_buckets,
            hashtable,
            _restored : false,  // this field doesn't matter as it won't be compared
        }
    }

    // Used in testing to compare `Seg`s
    #[cfg(test)]
    pub(crate) fn equivalent_seg(&self, s: Seg) -> bool {
        self.segments.equivalent_segments(s.segments) &&
        self.ttl_buckets.equivalent_ttlbuckets(s.ttl_buckets) &&
        self.hashtable.equivalent_hashtables(s.hashtable)
    }
    
}
