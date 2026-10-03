//! The sidecar images in flight.
//!
//! An NFSv3 server sees a sidecar as bytes at offsets, not as attributes:
//! the Mac creates `._x`, writes a 4 KiB header here and a resource fork
//! there, commits, and later reads the result back a piece at a time. The
//! image of each such file lives here between those steps, keyed by the
//! sidecar's handle. One with writes not yet applied to its target is
//! *dirty*: the write side applies it on `COMMIT`, on a stable write, and
//! when it is retired, once it parses as a whole and the target exists. A
//! dirty image whose target is not there yet — `cp -R` of a volume with
//! real `._` files writes the sidecar before the file it belongs to — waits
//! for the target until [`PENDING_TTL`] passes. A clean image is a cache
//! for the Mac's reads that follow, and goes after [`CLEAN_TTL`] idle. The
//! table is bounded in count and bytes; past either, the least recently
//! used go first.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use nfs3_server::nfs3_types::nfs3::nfsstat3;

/// The most a sidecar may grow to: a resource fork is at most 16 MiB in
/// Apple's own tools, and nothing else in a sidecar comes near.
pub const MAX_IMAGE_LEN: usize = 64 * 1024 * 1024;
/// Images kept at most.
const MAX_IMAGES: usize = 1024;
/// Bytes kept at most, across all images.
const MAX_TOTAL_LEN: usize = 128 * 1024 * 1024;
/// How long a clean image answers reads after its last use.
pub const CLEAN_TTL: Duration = Duration::from_secs(5);
/// How long a dirty image waits for its last piece or for its target.
pub const PENDING_TTL: Duration = Duration::from_secs(600);

/// One sidecar's bytes, as the Mac has written or last read them.
#[derive(Debug, Clone)]
pub struct Image {
    pub bytes: Vec<u8>,
    /// Permission bits the Mac set on the sidecar, if it did.
    pub mode: Option<u32>,
    pub modified: SystemTime,
    /// Written since last applied to the target.
    pub dirty: bool,
    /// Bumped by every write, so an apply that raced one does not mark
    /// the image clean.
    pub generation: u64,
    /// The generation last applied to a target that was not there: no
    /// use trying again until the Mac writes more.
    pub tried: Option<u64>,
    /// Born from a `CREATE`: the Mac believes the sidecar is new and
    /// writes only what it is adding, so the image is merged into the
    /// target's attributes rather than replacing them.
    pub merge: bool,
    last_used: Instant,
}

/// What a `GETATTR` of a sidecar needs from its image.
#[derive(Debug, Clone, Copy)]
pub struct Facts {
    pub len: u64,
    pub modified: SystemTime,
    pub mode: Option<u32>,
}

impl Image {
    /// An image of `bytes` as they stand on the target: clean, and the
    /// whole truth, so a rewrite of it replaces the target's attributes.
    pub fn synthesized(bytes: Vec<u8>, modified: SystemTime, now: Instant) -> Self {
        Self {
            bytes,
            mode: None,
            modified,
            dirty: false,
            generation: 0,
            tried: None,
            merge: false,
            last_used: now,
        }
    }

    /// An image not yet on its target: dirty, so that one never written
    /// stays out of the target and one holding content reaches it — merged
    /// when it does, since the Mac never saw what the target has.
    pub fn pending(bytes: Vec<u8>, now: Instant) -> Self {
        Self {
            bytes,
            mode: None,
            modified: SystemTime::now(),
            dirty: true,
            generation: 0,
            tried: None,
            merge: true,
            last_used: now,
        }
    }

    /// An image the Mac is about to write.
    pub fn fresh(now: Instant) -> Self {
        Self::pending(Vec::new(), now)
    }

    pub fn facts(&self) -> Facts {
        Facts {
            len: self.bytes.len() as u64,
            modified: self.modified,
            mode: self.mode,
        }
    }

    /// Whether the image is due to go at `now`.
    fn expired(&self, now: Instant) -> bool {
        let ttl = if self.dirty { PENDING_TTL } else { CLEAN_TTL };
        now.saturating_duration_since(self.last_used) >= ttl
    }
}

/// The images by sidecar handle; see the module docs.
#[derive(Debug)]
pub struct Images {
    by_id: HashMap<u64, Image>,
    total_len: usize,
    max_total_len: usize,
}

impl Default for Images {
    fn default() -> Self {
        Self {
            by_id: HashMap::new(),
            total_len: 0,
            max_total_len: MAX_TOTAL_LEN,
        }
    }
}

impl Images {
    pub fn contains(&self, id: u64) -> bool {
        self.by_id.contains_key(&id)
    }

    /// The image under `id`, counted as used now.
    pub fn get(&mut self, id: u64, now: Instant) -> Option<&Image> {
        let image = self.by_id.get_mut(&id)?;
        image.last_used = now;
        Some(image)
    }

    pub fn get_mut(&mut self, id: u64, now: Instant) -> Option<&mut Image> {
        let image = self.by_id.get_mut(&id)?;
        image.last_used = now;
        Some(image)
    }

    /// The image under `id` without counting a use: for the write side's
    /// own bookkeeping, which must not keep an image alive.
    pub fn peek(&self, id: u64) -> Option<&Image> {
        self.by_id.get(&id)
    }

    pub fn peek_mut(&mut self, id: u64) -> Option<&mut Image> {
        self.by_id.get_mut(&id)
    }

    /// Puts `image` under `id`, replacing what was there, and makes room
    /// for it; the dirty images that had to go come back for the caller to
    /// apply before they are lost.
    pub fn insert(&mut self, id: u64, image: Image, now: Instant) -> Vec<(u64, Image)> {
        self.remove(id);
        self.total_len += image.bytes.len();
        self.by_id.insert(id, image);
        self.evict(now, Some(id))
    }

    pub fn remove(&mut self, id: u64) -> Option<Image> {
        let image = self.by_id.remove(&id)?;
        self.total_len -= image.bytes.len();
        Some(image)
    }

    /// Writes `data` at `offset` into the image under `id`, growing it with
    /// zeros to reach the offset; `NFS3ERR_FBIG` past [`MAX_IMAGE_LEN`].
    pub fn write(
        &mut self,
        id: u64,
        offset: u64,
        data: &[u8],
        now: Instant,
    ) -> Result<&Image, nfsstat3> {
        let end = usize::try_from(offset)
            .ok()
            .and_then(|start| start.checked_add(data.len()))
            .filter(|&end| end <= MAX_IMAGE_LEN)
            .ok_or(nfsstat3::NFS3ERR_FBIG)?;
        let image = self.by_id.get_mut(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
        let start = end - data.len();
        if image.bytes.len() < end {
            self.total_len += end - image.bytes.len();
            image.bytes.resize(end, 0);
        }
        image.bytes[start..end].copy_from_slice(data);
        image.modified = SystemTime::now();
        image.dirty = true;
        image.generation += 1;
        image.last_used = now;
        Ok(image)
    }

    /// Sets the image's length, as a `SETATTR` of the size does.
    pub fn truncate(&mut self, id: u64, size: u64, now: Instant) -> Result<&Image, nfsstat3> {
        let size = usize::try_from(size)
            .ok()
            .filter(|&size| size <= MAX_IMAGE_LEN)
            .ok_or(nfsstat3::NFS3ERR_FBIG)?;
        let image = self.by_id.get_mut(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
        if size != image.bytes.len() {
            self.total_len = self.total_len - image.bytes.len() + size;
            image.bytes.resize(size, 0);
            image.modified = SystemTime::now();
            image.dirty = true;
            image.generation += 1;
        }
        image.last_used = now;
        Ok(image)
    }

    /// The dirty images with writes not yet tried against their targets
    /// and no write for `idle`: due to be applied where they are.
    pub fn due(&self, now: Instant, idle: Duration) -> Vec<u64> {
        self.by_id
            .iter()
            .filter(|(_, image)| {
                image.dirty
                    && image.tried != Some(image.generation)
                    && now.saturating_duration_since(image.last_used) >= idle
            })
            .map(|(&id, _)| id)
            .collect()
    }

    /// Takes out every image that is due at `now`, dirty ones included, so
    /// the caller can apply what it still can.
    pub fn retire(&mut self, now: Instant) -> Vec<(u64, Image)> {
        let due: Vec<u64> = self
            .by_id
            .iter()
            .filter(|(_, image)| image.expired(now))
            .map(|(&id, _)| id)
            .collect();
        let mut retired = Vec::with_capacity(due.len());
        for id in due {
            if let Some(image) = self.remove(id) {
                retired.push((id, image));
            }
        }
        retired.extend(self.evict(now, None));
        retired
    }

    /// Drops the least recently used images until the table is within its
    /// bounds, sparing `keep`, and returns the dirty ones.
    fn evict(&mut self, now: Instant, keep: Option<u64>) -> Vec<(u64, Image)> {
        let mut dirty = Vec::new();
        while self.by_id.len() > MAX_IMAGES || self.total_len > self.max_total_len {
            // Clean images first, the least recently used among them.
            let victim = self
                .by_id
                .iter()
                .filter(|(id, _)| Some(**id) != keep)
                .max_by_key(|(_, image)| {
                    (!image.dirty, now.saturating_duration_since(image.last_used))
                })
                .map(|(&id, _)| id);
            let Some(id) = victim else {
                break;
            };
            if let Some(image) = self.remove(id)
                && image.dirty
            {
                dirty.push((id, image));
            }
        }
        dirty
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    #[cfg(test)]
    fn with_byte_budget(max_total_len: usize) -> Self {
        Self {
            max_total_len,
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_grow_the_image_and_mark_it_dirty() {
        let mut images = Images::default();
        let now = Instant::now();
        images.insert(
            7,
            Image::synthesized(b"abc".to_vec(), SystemTime::now(), now),
            now,
        );
        assert!(!images.get(7, now).unwrap().dirty);
        let image = images.write(7, 5, b"xy", now).unwrap();
        assert_eq!(image.bytes, b"abc\0\0xy");
        assert!(image.dirty);
        assert_eq!(images.total_len, 7);
        images.truncate(7, 2, now).unwrap();
        assert_eq!(images.get(7, now).unwrap().bytes, b"ab");
        assert_eq!(images.total_len, 2);
        assert!(matches!(
            images.write(7, MAX_IMAGE_LEN as u64, b"!", now),
            Err(nfsstat3::NFS3ERR_FBIG)
        ));
        assert!(matches!(
            images.write(8, 0, b"!", now),
            Err(nfsstat3::NFS3ERR_STALE)
        ));
    }

    #[test]
    fn clean_images_go_soon_and_dirty_ones_wait() {
        let mut images = Images::default();
        let start = Instant::now();
        images.insert(
            1,
            Image::synthesized(Vec::new(), SystemTime::now(), start),
            start,
        );
        images.insert(2, Image::fresh(start), start);
        let second = Duration::from_secs(1);
        assert_eq!(images.due(start, second), Vec::<u64>::new());
        assert_eq!(images.due(start + second, second), [2]);
        images.get_mut(2, start).unwrap().tried = Some(0);
        assert_eq!(
            images.due(start + second, second),
            Vec::<u64>::new(),
            "tried at this generation"
        );
        images.write(2, 0, b"x", start).unwrap();
        assert_eq!(images.due(start + second, second), [2]);
        assert!(
            images
                .retire(start + CLEAN_TTL.checked_sub(Duration::from_millis(1)).unwrap())
                .is_empty()
        );
        let retired = images.retire(start + CLEAN_TTL);
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].0, 1);
        assert!(images.contains(2));
        let retired = images.retire(start + PENDING_TTL);
        assert_eq!(retired.len(), 1);
        assert!(
            retired[0].1.dirty,
            "a dirty image comes back for one last try"
        );
        assert_eq!(images.len(), 0);
    }

    #[test]
    fn the_table_is_bounded_and_clean_images_go_first() {
        let mut images = Images::default();
        let now = Instant::now();
        for id in 0..MAX_IMAGES as u64 {
            let used = now + Duration::from_millis(id);
            let image = if id % 2 == 0 {
                Image::fresh(used)
            } else {
                Image::synthesized(Vec::new(), SystemTime::now(), used)
            };
            images.insert(id, image, used);
        }
        let later = now + Duration::from_secs(1);
        let evicted = images.insert(5000, Image::fresh(later), later);
        assert_eq!(images.len(), MAX_IMAGES);
        assert!(evicted.is_empty(), "a clean image went, not a dirty one");
        assert!(!images.contains(1), "the least recently used clean one");
        assert!(images.contains(5000));

        // Over the byte budget, clean images go first, then dirty ones,
        // oldest first; the newcomer always stays.
        let mut images = Images::with_byte_budget(1000);
        let mut at = now;
        for id in 0..4u64 {
            at += Duration::from_secs(1);
            let mut image = if id == 1 {
                Image::synthesized(Vec::new(), SystemTime::now(), at)
            } else {
                Image::fresh(at)
            };
            image.bytes = vec![0; 300];
            images.insert(id, image, at);
        }
        assert_eq!(images.len(), 3, "the clean one made room");
        let mut big = Image::fresh(at);
        big.bytes = vec![0; 900];
        let evicted = images.insert(9, big, at);
        assert_eq!(images.len(), 1, "only the newcomer fits");
        assert!(images.contains(9));
        let mut evicted_ids: Vec<u64> = evicted.iter().map(|(id, _)| *id).collect();
        evicted_ids.sort_unstable();
        assert_eq!(evicted_ids, [0, 2, 3]);
        assert!(evicted.iter().all(|(_, image)| image.dirty));
    }
}
