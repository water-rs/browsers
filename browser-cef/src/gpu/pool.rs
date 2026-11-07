//! The reuse policy of the textures CEF frames are copied into.
//!
//! A frame is presented to the host as is, so the texture behind it must stay
//! untouched until the host is done with it, and on macOS until the system
//! compositor is too. A [`SlotPool`] hands out slots under that rule and knows
//! nothing about how a slot is allocated: the platform supplies the slot type.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A texture a CEF frame is copied into, as the reuse policy sees it.
pub(super) trait PoolSlot {
    /// The pixel extent of the slot.
    fn extent(&self) -> (u32, u32);

    /// The pixel format of the slot.
    fn format(&self) -> wgpu::TextureFormat;

    /// Whether the system still shows or reads the slot's memory, which can
    /// outlast the host's release of the frame: a macOS compositor plane keeps
    /// displaying an `IOSurface` after the engine has dropped it.
    fn in_system_use(&self) -> bool;
}

/// A counted claim on one slot. A slot is reusable only while no claim on it
/// is alive, so whoever still needs its pixels — a frame the host has not
/// released, the copy a popup is composed over — holds one.
#[derive(Debug)]
pub(super) struct Claim(Arc<AtomicUsize>);

impl Claim {
    fn new(claims: &Arc<AtomicUsize>) -> Self {
        claims.fetch_add(1, Ordering::AcqRel);
        Self(Arc::clone(claims))
    }
}

impl Clone for Claim {
    fn clone(&self) -> Self {
        Self::new(&self.0)
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Entry<S> {
    slot: S,
    claims: Arc<AtomicUsize>,
}

impl<S> Entry<S> {
    fn is_free(&self) -> bool {
        self.claims.load(Ordering::Acquire) == 0
    }
}

/// The slots one page's frames are copied into.
pub(super) struct SlotPool<S> {
    entries: Vec<Entry<S>>,
}

impl<S> Default for SlotPool<S> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<S: PoolSlot> SlotPool<S> {
    /// A slot of `extent` and `format` nobody else holds, with the first claim
    /// on it.
    ///
    /// A free slot of another size or format is dropped here, so a resize
    /// leaves nothing behind; a free slot of the right shape is reused unless
    /// the system still uses it, and `allocate` makes a new one otherwise.
    pub(super) fn acquire(
        &mut self,
        extent: (u32, u32),
        format: wgpu::TextureFormat,
        allocate: impl FnOnce() -> S,
    ) -> (&S, Claim) {
        self.entries.retain(|entry| {
            !entry.is_free() || (entry.slot.extent() == extent && entry.slot.format() == format)
        });
        let index = if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.is_free() && !entry.slot.in_system_use())
        {
            index
        } else {
            self.entries.push(Entry {
                slot: allocate(),
                claims: Arc::new(AtomicUsize::new(0)),
            });
            self.entries.len() - 1
        };
        let entry = &self.entries[index];
        (&entry.slot, Claim::new(&entry.claims))
    }

    #[cfg(test)]
    const fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{PoolSlot, SlotPool};

    const SIZE: (u32, u32) = (64, 32);
    const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

    struct FakeSlot {
        extent: (u32, u32),
        format: wgpu::TextureFormat,
        in_system_use: Arc<AtomicBool>,
    }

    impl FakeSlot {
        fn new(extent: (u32, u32), format: wgpu::TextureFormat) -> (Self, Arc<AtomicBool>) {
            let in_system_use = Arc::new(AtomicBool::new(false));
            (
                Self {
                    extent,
                    format,
                    in_system_use: Arc::clone(&in_system_use),
                },
                in_system_use,
            )
        }
    }

    impl PoolSlot for FakeSlot {
        fn extent(&self) -> (u32, u32) {
            self.extent
        }

        fn format(&self) -> wgpu::TextureFormat {
            self.format
        }

        fn in_system_use(&self) -> bool {
            self.in_system_use.load(Ordering::Relaxed)
        }
    }

    fn fresh(extent: (u32, u32), format: wgpu::TextureFormat) -> FakeSlot {
        FakeSlot::new(extent, format).0
    }

    #[test]
    fn a_held_slot_is_never_handed_out_again() {
        let mut pool = SlotPool::default();
        let (_, first) = pool.acquire(SIZE, FORMAT, || fresh(SIZE, FORMAT));
        let (_, second) = pool.acquire(SIZE, FORMAT, || fresh(SIZE, FORMAT));
        assert_eq!(pool.len(), 2, "the held slot forced a second allocation");
        drop((first, second));
    }

    #[test]
    fn a_released_slot_of_the_same_shape_is_reused() {
        let mut pool = SlotPool::default();
        let (_, claim) = pool.acquire(SIZE, FORMAT, || fresh(SIZE, FORMAT));
        drop(claim);
        let (_, _claim) = pool.acquire(SIZE, FORMAT, || {
            panic!("a free slot of the right shape must be reused")
        });
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn a_slot_stays_busy_until_every_claim_on_it_is_gone() {
        let mut pool = SlotPool::default();
        let (_, claim) = pool.acquire(SIZE, FORMAT, || fresh(SIZE, FORMAT));
        let shared = claim.clone();
        drop(claim);
        let (_, other) = pool.acquire(SIZE, FORMAT, || fresh(SIZE, FORMAT));
        assert_eq!(pool.len(), 2, "one claim left, so the slot is still held");
        drop((shared, other));
        let (_, _claim) = pool.acquire(SIZE, FORMAT, || {
            panic!("both slots are free and the first one is reused")
        });
    }

    #[test]
    fn a_free_slot_of_a_stale_size_is_dropped() {
        let mut pool = SlotPool::default();
        let (_, claim) = pool.acquire(SIZE, FORMAT, || fresh(SIZE, FORMAT));
        drop(claim);
        let resized = (SIZE.0 * 2, SIZE.1);
        let (slot, _claim) = pool.acquire(resized, FORMAT, || fresh(resized, FORMAT));
        assert_eq!(slot.extent(), resized);
        assert_eq!(pool.len(), 1, "the stale slot did not survive the resize");
    }

    #[test]
    fn a_free_slot_of_another_format_is_dropped() {
        let mut pool = SlotPool::default();
        let (_, claim) = pool.acquire(SIZE, FORMAT, || fresh(SIZE, FORMAT));
        drop(claim);
        let other = wgpu::TextureFormat::Rgba8Unorm;
        let (slot, _claim) = pool.acquire(SIZE, other, || fresh(SIZE, other));
        assert_eq!(slot.format(), other);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn a_held_slot_of_a_stale_size_is_kept_until_it_is_released() {
        let mut pool = SlotPool::default();
        let (_, claim) = pool.acquire(SIZE, FORMAT, || fresh(SIZE, FORMAT));
        let resized = (SIZE.0 * 2, SIZE.1);
        let (_, _new) = pool.acquire(resized, FORMAT, || fresh(resized, FORMAT));
        assert_eq!(pool.len(), 2, "the host still shows the old slot");
        drop(claim);
        let (_, _new) = pool.acquire(resized, FORMAT, || fresh(resized, FORMAT));
        assert_eq!(pool.len(), 2, "the old slot went once it was released");
    }

    #[test]
    fn a_slot_the_system_still_uses_is_not_reused_until_it_stops() {
        let mut pool = SlotPool::default();
        let (slot, in_system_use) = FakeSlot::new(SIZE, FORMAT);
        let (_, claim) = pool.acquire(SIZE, FORMAT, || slot);
        in_system_use.store(true, Ordering::Relaxed);
        drop(claim);

        let (_, other) = pool.acquire(SIZE, FORMAT, || fresh(SIZE, FORMAT));
        assert_eq!(pool.len(), 2, "released, but the compositor still shows it");
        drop(other);

        in_system_use.store(false, Ordering::Relaxed);
        let (_, _claim) = pool.acquire(SIZE, FORMAT, || {
            panic!("the slots are free and out of system use")
        });
        assert_eq!(pool.len(), 2);
    }
}
