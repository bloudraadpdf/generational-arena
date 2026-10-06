use super::{vec, Arc, Entry, Vec};
use core::convert::TryFrom;
use core::{cmp, iter, mem, ops, slice};

const CHUNK_BYTES: usize = 4 * 1024;

/// The slots of an arena in chunks of about `CHUNK_BYTES`. Each chunk but the last is full, so the arena grows
/// without a copy of its slots and keeps less than one chunk of spare slots. A clone shares each chunk until one of
/// the two changes it. A chunk whose slots are all released keeps no entry: its slots stay taken, and its memory goes
/// back.
#[derive(Clone, Debug)]
pub(crate) struct Slots<T> {
    chunks: Vec<Chunk<T>>,
    released: Vec<u16>,
}

type Chunk<T> = Arc<[Entry<T>]>;
type Placed<I> = iter::Zip<ops::Range<usize>, I>;
type Shared<'a, T> = fn((usize, &'a Chunk<T>)) -> Placed<slice::Iter<'a, Entry<T>>>;
type Unique<'a, T> = fn((usize, &'a mut Chunk<T>)) -> Placed<slice::IterMut<'a, Entry<T>>>;
type Owned<T> = fn((usize, Chunk<T>)) -> Placed<vec::IntoIter<Entry<T>>>;
type Take<T> = fn(Chunk<T>) -> Vec<Entry<T>>;

pub(crate) type SlotIter<'a, T> =
    iter::FlatMap<iter::Enumerate<slice::Iter<'a, Chunk<T>>>, Placed<slice::Iter<'a, Entry<T>>>, Shared<'a, T>>;
pub(crate) type SlotIterMut<'a, T> =
    iter::FlatMap<iter::Enumerate<slice::IterMut<'a, Chunk<T>>>, Placed<slice::IterMut<'a, Entry<T>>>, Unique<'a, T>>;
pub(crate) type SlotDrain<'a, T> =
    iter::FlatMap<iter::Enumerate<vec::Drain<'a, Chunk<T>>>, Placed<vec::IntoIter<Entry<T>>>, Owned<T>>;
pub(crate) type SlotIntoIter<T> = iter::Flatten<iter::Map<vec::IntoIter<Chunk<T>>, Take<T>>>;

impl<T> Slots<T> {
    const CHUNK_LEN: usize = CHUNK_BYTES.div_ceil(mem::size_of::<Entry<T>>());

    pub(crate) fn new() -> Slots<T> {
        Slots {
            chunks: Vec::new(),
            released: Vec::new(),
        }
    }

    fn position(slot: usize) -> (usize, usize) {
        (slot / Self::CHUNK_LEN, slot % Self::CHUNK_LEN)
    }

    /// The slots of `chunk`: a released chunk keeps no entry and takes a full chunk of slots.
    fn chunk_len(chunk: &Chunk<T>) -> usize {
        if chunk.is_empty() {
            Self::CHUNK_LEN
        } else {
            chunk.len()
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.chunks
            .last()
            .map_or(0, |last| (self.chunks.len() - 1) * Self::CHUNK_LEN + Self::chunk_len(last))
    }

    /// The slots to add to a full arena: the length again, up to the end of the last chunk.
    pub(crate) fn growth(&self) -> usize {
        let len = self.len();
        cmp::min(cmp::max(len, 1), Self::CHUNK_LEN - len % Self::CHUNK_LEN)
    }

    /// The slots up to the last slot that is not free; a released slot is not free.
    pub(crate) fn taken_len(&self) -> usize {
        self.chunks
            .iter()
            .enumerate()
            .rev()
            .find_map(|(chunk, entries)| {
                let start = chunk * Self::CHUNK_LEN;
                if entries.is_empty() {
                    return Some(start + Self::CHUNK_LEN);
                }
                entries
                    .iter()
                    .rposition(|entry| !matches!(entry, Entry::Free { .. }))
                    .map(|last| start + last + 1)
            })
            .unwrap_or(0)
    }

    pub(crate) fn get(&self, slot: usize) -> Option<&Entry<T>> {
        let (chunk, offset) = Self::position(slot);
        self.chunks.get(chunk)?.get(offset)
    }

    pub(crate) fn iter(&self) -> SlotIter<'_, T> {
        let shared: Shared<'_, T> = |(chunk, entries)| Self::placed(chunk, entries.iter());
        self.chunks.iter().enumerate().flat_map(shared)
    }

    fn placed<I: ExactSizeIterator>(chunk: usize, entries: I) -> Placed<I> {
        let start = chunk * Self::CHUNK_LEN;
        (start..start + entries.len()).zip(entries)
    }
}

impl<T: Clone> Slots<T> {
    pub(crate) fn get_mut(&mut self, slot: usize) -> Option<&mut Entry<T>> {
        let (chunk, offset) = Self::position(slot);
        Self::entries_mut(self.chunks.get_mut(chunk)?)?.get_mut(offset)
    }

    /// The entries of 2 distinct slots, or `None` for a slot without an entry.
    pub(crate) fn pair_mut(&mut self, a: usize, b: usize) -> [Option<&mut Entry<T>>; 2] {
        let ((chunk_a, offset_a), (chunk_b, offset_b)) = (Self::position(a), Self::position(b));
        if chunk_a == chunk_b {
            let Some(entries) = self.chunks.get_mut(chunk_a).and_then(Self::entries_mut) else {
                return [None, None];
            };
            let len = entries.len();
            if offset_a >= len {
                return [None, entries.get_mut(offset_b)];
            }
            if offset_b >= len {
                return [entries.get_mut(offset_a), None];
            }
            let [a, b] = entries
                .get_disjoint_mut([offset_a, offset_b])
                .expect("two distinct slots");
            return [Some(a), Some(b)];
        }
        let chunks = self.chunks.len();
        if chunk_a >= chunks {
            return [None, self.get_mut(b)];
        }
        if chunk_b >= chunks {
            return [self.get_mut(a), None];
        }
        let [x, y] = self
            .chunks
            .get_disjoint_mut([chunk_a, chunk_b])
            .expect("two distinct chunks");
        [
            Self::entries_mut(x).and_then(|entries| entries.get_mut(offset_a)),
            Self::entries_mut(y).and_then(|entries| entries.get_mut(offset_b)),
        ]
    }

    /// The entries of `chunk`, or `None` for a released chunk.
    fn entries_mut(chunk: &mut Chunk<T>) -> Option<&mut [Entry<T>]> {
        if chunk.is_empty() {
            None
        } else {
            Some(Arc::make_mut(chunk))
        }
    }

    pub(crate) fn extend(&mut self, mut entries: impl ExactSizeIterator<Item = Entry<T>>) {
        while entries.len() > 0 {
            let kept = if self
                .chunks
                .last()
                .is_some_and(|last| !last.is_empty() && last.len() < Self::CHUNK_LEN)
            {
                self.chunks.pop().map_or_else(Vec::new, take)
            } else {
                Vec::new()
            };
            let room = cmp::min(entries.len(), Self::CHUNK_LEN - kept.len());
            self.chunks
                .push(kept.into_iter().chain(entries.by_ref().take(room)).collect());
        }
        self.released.resize(self.chunks.len(), 0);
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        let (full, rest) = Self::position(len);
        self.chunks.truncate(full + usize::from(rest > 0));
        self.released.truncate(self.chunks.len());
        if self.chunks.get(full).is_some_and(|last| last.len() > rest) {
            let mut last = self.chunks.pop().map_or_else(Vec::new, take);
            last.truncate(rest);
            self.released[full] = released_count(&last);
            self.chunks.push(last.into());
        }
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.chunks.shrink_to_fit();
        self.released.shrink_to_fit();
    }

    /// Releases the entry of `slot` and gives it back. A chunk whose slots are all released gives back its memory.
    /// Panics for a slot without an entry.
    pub(crate) fn release(&mut self, slot: usize) -> Entry<T> {
        let entry = mem::replace(&mut self[slot], Entry::Released);
        let chunk = slot / Self::CHUNK_LEN;
        self.released[chunk] += 1;
        if usize::from(self.released[chunk]) == Self::CHUNK_LEN {
            self.chunks[chunk] = Arc::new([]);
        }
        entry
    }

    /// Frees each slot that is not released.
    pub(crate) fn free_all(&mut self) {
        for (_, entry) in self.iter_mut() {
            if !matches!(entry, Entry::Released) {
                *entry = Entry::Free { next_free: None };
            }
        }
    }

    pub(crate) fn iter_mut(&mut self) -> SlotIterMut<'_, T> {
        let unique: Unique<'_, T> = |(chunk, entries)| {
            Self::placed(chunk, Self::entries_mut(entries).unwrap_or_default().iter_mut())
        };
        self.chunks.iter_mut().enumerate().flat_map(unique)
    }

    pub(crate) fn drain(&mut self) -> SlotDrain<'_, T> {
        self.released.clear();
        let owned: Owned<T> = |(chunk, entries)| Self::placed(chunk, take(entries).into_iter());
        self.chunks.drain(..).enumerate().flat_map(owned)
    }
}

/// The entries of `chunk`, without a copy if no other arena shares it.
fn take<T: Clone>(mut chunk: Chunk<T>) -> Vec<Entry<T>> {
    if chunk.is_empty() {
        return Vec::new();
    }
    Arc::make_mut(&mut chunk)
        .iter_mut()
        .map(|entry| mem::replace(entry, Entry::Free { next_free: None }))
        .collect()
}

fn released_count<T>(entries: &[Entry<T>]) -> u16 {
    let count = entries
        .iter()
        .filter(|entry| matches!(entry, Entry::Released))
        .count();
    u16::try_from(count).expect("a chunk holds fewer than 2^16 slots")
}

impl<T: Clone> IntoIterator for Slots<T> {
    type Item = Entry<T>;
    type IntoIter = SlotIntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.chunks.into_iter().map(take as Take<T>).flatten()
    }
}

impl<T> ops::Index<usize> for Slots<T> {
    type Output = Entry<T>;

    fn index(&self, slot: usize) -> &Entry<T> {
        let (chunk, offset) = Self::position(slot);
        &self.chunks[chunk][offset]
    }
}

impl<T: Clone> ops::IndexMut<usize> for Slots<T> {
    fn index_mut(&mut self, slot: usize) -> &mut Entry<T> {
        let (chunk, offset) = Self::position(slot);
        &mut Arc::make_mut(&mut self.chunks[chunk])[offset]
    }
}
