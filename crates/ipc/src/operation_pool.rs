//! A fixed population of native operation records in acquired storage.
//!
//! Decision: docs/research/managed-native-bounded-records-decision-v1.md. The
//! population is the owner's declared `pending_operations`, never a default.
//! A slot is `Free`, `Live` (the operation's record and every obligation it
//! still carries), or `Retired` (only its ID, kept for duplicate refusal until
//! a fence covers it). A fence certifies that first publication is resolved for
//! every ID at or below it, so a later Prepare there is a duplicate and retired
//! slots there can be reused. A Live record is never freed by a fence.

use std::marker::PhantomData;
use std::mem::{align_of, size_of};
use std::num::{NonZeroU64, NonZeroUsize};

use crate::locked_region::{self, Region, Retained};

/// One slot. `Live` records stay where they are until their owner retires them.
#[derive(Debug)]
pub enum Entry<T> {
    Free,
    Live { id: NonZeroU64, record: T },
    Retired { id: NonZeroU64 },
}

/// Why the pool refused an operation; callers map these onto their own wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The ID is at or below the fence, or already holds a slot.
    Duplicate,
    /// Every slot is Live, or Retired above the fence.
    ResourcesUnavailable,
    /// A fence below the current one.
    State,
    /// No Live record has this ID.
    Identity,
    /// The storage is not borrowable here (not the acquiring address space).
    Storage(locked_region::Failure),
}

/// Why acquisition failed.
#[derive(Debug)]
pub enum Failure {
    /// The population or its byte size is not representable here.
    Layout,
    /// The locked region refused, or its cleanup failed and retains the mapping.
    Storage(locked_region::Rejected),
    /// The acquired region could not be prepared and its release failed too.
    Retained(Retained),
}

/// Why `release` did not unmap the pool.
#[derive(Debug)]
pub enum Release<T> {
    /// Live records remain; the pool is handed back unchanged.
    Live(Pool<T>),
    /// The storage is not borrowable here; the pool is handed back unchanged.
    NotOwner(Pool<T>, locked_region::Failure),
    /// The unmap failed; the mapping stays owned by this value.
    Storage(Retained),
}

/// The population's records in one locked region, all slots initialized.
pub struct Pool<T> {
    region: Region,
    population: NonZeroUsize,
    /// The highest certified fence, 0 before any (IDs are nonzero).
    fence: u64,
    _records: PhantomData<T>,
}

impl<T> std::fmt::Debug for Pool<T> {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("OperationPool")
            .field("population", &self.population)
            .field("fence", &self.fence)
            .finish_non_exhaustive()
    }
}

impl<T> Pool<T> {
    /// Acquires `population` slots of `Entry<T>` and initializes every one Free.
    pub fn acquire(population: NonZeroU64) -> Result<Self, Failure> {
        let population = usize::try_from(population.get())
            .ok()
            .and_then(NonZeroUsize::new)
            .ok_or(Failure::Layout)?;
        let bytes = population
            .get()
            .checked_mul(size_of::<Entry<T>>())
            .and_then(NonZeroUsize::new)
            .ok_or(Failure::Layout)?;
        let mut region = Region::acquire_zeroed(bytes).map_err(Failure::Storage)?;
        let base = match region.bytes_mut() {
            Ok(bytes) => bytes.as_mut_ptr(),
            Err(_) => return Err(release_failed(region)),
        };
        // mmap returns page-aligned memory; refuse an alignment it cannot meet.
        if base.align_offset(align_of::<Entry<T>>()) != 0 {
            return Err(release_failed(region));
        }
        let first = base.cast::<Entry<T>>();
        for index in 0..population.get() {
            // SAFETY: `index < population`, and the region's usable bytes are
            // exactly `population * size_of::<Entry<T>>()` at an aligned base, so
            // each slot is in bounds; none is initialized yet, so `write` drops nothing.
            unsafe { first.add(index).write(Entry::Free) };
        }
        Ok(Self {
            region,
            population,
            fence: 0,
            _records: PhantomData,
        })
    }

    pub fn population(&self) -> NonZeroUsize {
        self.population
    }

    /// The highest certified fence, 0 before any.
    pub fn fenced_through(&self) -> u64 {
        self.fence
    }

    fn entries(&mut self) -> Result<&mut [Entry<T>], locked_region::Failure> {
        let population = self.population.get();
        let bytes = self.region.bytes_mut()?;
        let first = bytes.as_mut_ptr().cast::<Entry<T>>();
        // SAFETY: `acquire` initialized exactly `population` aligned entries in
        // these bytes and every later write goes through this typed view; the
        // exclusive borrow of `self` keeps the bytes unaliased for its lifetime.
        Ok(unsafe { std::slice::from_raw_parts_mut(first, population) })
    }

    /// Admits operation `id` with `record`, or hands the record back refused.
    pub fn prepare(&mut self, id: NonZeroU64, record: T) -> Result<(), (Refusal, T)> {
        if id.get() <= self.fence {
            return Err((Refusal::Duplicate, record));
        }
        let entries = match self.entries() {
            Ok(entries) => entries,
            Err(failure) => return Err((Refusal::Storage(failure), record)),
        };
        let mut free = None;
        for (index, entry) in entries.iter().enumerate() {
            match entry {
                Entry::Live { id: held, .. } | Entry::Retired { id: held } if *held == id => {
                    return Err((Refusal::Duplicate, record));
                }
                Entry::Free if free.is_none() => free = Some(index),
                _ => {}
            }
        }
        match free.and_then(|index| entries.get_mut(index)) {
            Some(slot) => {
                *slot = Entry::Live { id, record };
                Ok(())
            }
            None => Err((Refusal::ResourcesUnavailable, record)),
        }
    }

    /// The Live record for `id`, including a Live record at or below the fence.
    pub fn get_mut(&mut self, id: NonZeroU64) -> Result<&mut T, Refusal> {
        let entries = self.entries().map_err(Refusal::Storage)?;
        entries
            .iter_mut()
            .find_map(|entry| match entry {
                Entry::Live { id: held, record } if *held == id => Some(record),
                _ => None,
            })
            .ok_or(Refusal::Identity)
    }

    /// Ends `id`'s Live record, keeping its ID for duplicate refusal, and hands
    /// the record to the caller, which owns whatever cleanup it still carries.
    pub fn retire(&mut self, id: NonZeroU64) -> Result<T, Refusal> {
        let entries = self.entries().map_err(Refusal::Storage)?;
        let slot = entries
            .iter_mut()
            .find(|entry| matches!(entry, Entry::Live { id: held, .. } if *held == id))
            .ok_or(Refusal::Identity)?;
        match std::mem::replace(slot, Entry::Retired { id }) {
            Entry::Live { record, .. } => Ok(record),
            other => {
                *slot = other;
                Err(Refusal::Identity)
            }
        }
    }

    /// Certifies resolved first publication through `through`: Retired slots at
    /// or below it become Free, and later Prepares at or below it are duplicates.
    pub fn fence(&mut self, through: NonZeroU64) -> Result<(), Refusal> {
        if through.get() < self.fence {
            return Err(Refusal::State);
        }
        let entries = self.entries().map_err(Refusal::Storage)?;
        for entry in entries.iter_mut() {
            if matches!(entry, Entry::Retired { id } if id.get() <= through.get()) {
                *entry = Entry::Free;
            }
        }
        self.fence = through.get();
        Ok(())
    }

    /// Every Live record, at or below the fence or not.
    pub fn live_mut(&mut self) -> Result<impl Iterator<Item = (NonZeroU64, &mut T)>, Refusal> {
        let entries = self.entries().map_err(Refusal::Storage)?;
        Ok(entries.iter_mut().filter_map(|entry| match entry {
            Entry::Live { id, record } => Some((*id, record)),
            _ => None,
        }))
    }

    /// (Live, Retired, Free) slot counts; they always sum to the population.
    pub fn counts(&mut self) -> Result<(usize, usize, usize), Refusal> {
        let entries = self.entries().map_err(Refusal::Storage)?;
        Ok(entries
            .iter()
            .fold((0, 0, 0), |(live, retired, free), entry| match entry {
                Entry::Live { .. } => (live.saturating_add(1), retired, free),
                Entry::Retired { .. } => (live, retired.saturating_add(1), free),
                Entry::Free => (live, retired, free.saturating_add(1)),
            }))
    }

    /// Unmaps the pool once no Live record remains; otherwise hands it back.
    pub fn release(mut self) -> Result<(), Release<T>> {
        match self.counts() {
            Ok((0, _, _)) => {}
            Ok(_) => return Err(Release::Live(self)),
            Err(Refusal::Storage(failure)) => return Err(Release::NotOwner(self, failure)),
            Err(_) => return Err(Release::Live(self)),
        }
        let region = self.take_region();
        region.release().map_err(Release::Storage)
    }

    /// Moves the region out after every entry is known to hold no record.
    fn take_region(self) -> Region {
        let this = std::mem::ManuallyDrop::new(self);
        // SAFETY: `this` is never used or dropped again; reading the region out
        // moves its sole owner. Free and Retired entries own nothing to drop.
        unsafe { std::ptr::read(&raw const this.region) }
    }
}

impl<T> Drop for Pool<T> {
    fn drop(&mut self) {
        // Drop is best effort, as the region's is: Live records drop here in the
        // acquiring process only; elsewhere the storage is not this process's.
        if let Ok(entries) = self.entries() {
            for entry in entries.iter_mut() {
                *entry = Entry::Free;
            }
        }
    }
}

/// Releases a just-acquired region that could not be prepared.
fn release_failed(region: Region) -> Failure {
    match region.release() {
        Ok(()) => Failure::Layout,
        Err(retained) => Failure::Retained(retained),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests;
