use super::{vec, Arc, Entry, Vec};
use core::convert::TryFrom;
use core::{cmp, iter, mem, ops, slice};

const CHUNK_BYTES: usize = 4 * 1024;

/// The slots of an arena in chunks of about `CHUNK_BYTES`. Each chunk but the last is full, so the arena grows
/// without a copy of its slots and keeps less than one chunk of spare slots. A clone shares each chunk until one of
/// the two changes it. A chunk whose slots are all released gives back its memory: it becomes the 1 chunk of released
/// entries that each such chunk of the arena shares.
#[derive(Clone, Debug)]
pub(crate) struct Slots<T> {
    chunks: Vec<Chunk<T>>,
    released: Vec<u16>,
    released_chunk: Option<Chunk<T>>,
}

type Chunk<T> = Arc<[Entry<T>]>;
type Shared<'a, T> = fn(&'a Chunk<T>) -> &'a [Entry<T>];
type Owned<T> = fn((usize, Chunk<T>)) -> Cursor<vec::IntoIter<Entry<T>>>;
type Take<T> = fn(Chunk<T>) -> Vec<Entry<T>>;

pub(crate) type SlotIter<'a, T> = Positions<iter::Flatten<iter::Map<slice::Iter<'a, Chunk<T>>, Shared<'a, T>>>>;
pub(crate) type SlotIterMut<'a, T> = ChunkPositions<ChunksMut<'a, T>, slice::IterMut<'a, Entry<T>>>;
pub(crate) type SlotDrain<'a, T> =
    ChunkPositions<iter::Map<iter::Enumerate<vec::Drain<'a, Chunk<T>>>, Owned<T>>, vec::IntoIter<Entry<T>>>;
pub(crate) type SlotIntoIter<T> = iter::Flatten<iter::Map<vec::IntoIter<Chunk<T>>, Take<T>>>;

impl<T> Slots<T> {
    const CHUNK_LEN: usize = CHUNK_BYTES.div_ceil(mem::size_of::<Entry<T>>());

    pub(crate) fn new() -> Slots<T> {
        Slots {
            chunks: Vec::new(),
            released: Vec::new(),
            released_chunk: None,
        }
    }

    fn position(slot: usize) -> (usize, usize) {
        (slot / Self::CHUNK_LEN, slot % Self::CHUNK_LEN)
    }

    pub(crate) fn len(&self) -> usize {
        self.chunks
            .last()
            .map_or(0, |last| (self.chunks.len() - 1) * Self::CHUNK_LEN + last.len())
    }

    /// The slots to add to a full arena: the length again, up to the end of the last chunk.
    pub(crate) fn growth(&self) -> usize {
        let len = self.len();
        cmp::min(cmp::max(len, 1), Self::CHUNK_LEN - len % Self::CHUNK_LEN)
    }

    /// The slots up to the last slot that is not free; a released slot is not free.
    pub(crate) fn taken_len(&self) -> usize {
        self.iter()
            .rev()
            .find(|(_, entry)| !matches!(entry, Entry::Free { .. }))
            .map_or(0, |(last, _)| last + 1)
    }

    pub(crate) fn get(&self, slot: usize) -> Option<&Entry<T>> {
        let (chunk, offset) = Self::position(slot);
        self.chunks.get(chunk)?.get(offset)
    }

    pub(crate) fn iter(&self) -> SlotIter<'_, T> {
        let shared: Shared<'_, T> = AsRef::as_ref;
        Positions::new(self.len(), self.chunks.iter().map(shared).flatten())
    }

    fn cursor<I>(chunk: usize, entries: I) -> Cursor<I> {
        Cursor {
            position: chunk * Self::CHUNK_LEN,
            entries,
        }
    }

    fn is_released(released_chunk: Option<&Chunk<T>>, chunk: &Chunk<T>) -> bool {
        released_chunk.is_some_and(|released| Arc::ptr_eq(released, chunk))
    }

    /// Replaces each released chunk with a chunk without entries, which keeps its place in the positions of a
    /// `ChunkPositions`.
    fn forget_released_chunks(&mut self) {
        if let Some(released) = self.released_chunk.take() {
            for chunk in &mut self.chunks {
                if Arc::ptr_eq(chunk, &released) {
                    *chunk = Arc::new([]);
                }
            }
        }
    }
}

impl<T: Clone> Slots<T> {
    pub(crate) fn get_mut(&mut self, slot: usize) -> Option<&mut Entry<T>> {
        let (chunk, offset) = Self::position(slot);
        Self::entries_mut(self.released_chunk.as_ref(), self.chunks.get_mut(chunk)?)?.get_mut(offset)
    }

    /// The entries of 2 distinct slots, or `None` for a slot without an entry.
    pub(crate) fn pair_mut(&mut self, a: usize, b: usize) -> [Option<&mut Entry<T>>; 2] {
        let ((chunk_a, offset_a), (chunk_b, offset_b)) = (Self::position(a), Self::position(b));
        let released = self.released_chunk.as_ref();
        if chunk_a == chunk_b {
            let Some(entries) = self
                .chunks
                .get_mut(chunk_a)
                .and_then(|chunk| Self::entries_mut(released, chunk))
            else {
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
            Self::entries_mut(released, x).and_then(|entries| entries.get_mut(offset_a)),
            Self::entries_mut(released, y).and_then(|entries| entries.get_mut(offset_b)),
        ]
    }

    /// The entries of `chunk`, or `None` for a released chunk.
    fn entries_mut<'c>(released_chunk: Option<&Chunk<T>>, chunk: &'c mut Chunk<T>) -> Option<&'c mut [Entry<T>]> {
        if Self::is_released(released_chunk, chunk) {
            None
        } else {
            Some(Arc::make_mut(chunk))
        }
    }

    pub(crate) fn extend(&mut self, mut entries: impl ExactSizeIterator<Item = Entry<T>>) {
        while entries.len() > 0 {
            let kept = if self.chunks.last().is_some_and(|last| last.len() < Self::CHUNK_LEN) {
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

    /// Releases the entry of `slot` and gives it back. A full chunk whose slots are all released gives back its
    /// memory. Panics for a slot without an entry.
    pub(crate) fn release(&mut self, slot: usize) -> Entry<T> {
        let entry = mem::replace(&mut self[slot], Entry::Released);
        let chunk = slot / Self::CHUNK_LEN;
        self.released[chunk] += 1;
        if usize::from(self.released[chunk]) == Self::CHUNK_LEN {
            let released = self
                .released_chunk
                .get_or_insert_with(|| iter::repeat_with(|| Entry::Released).take(Self::CHUNK_LEN).collect());
            self.chunks[chunk] = Arc::clone(released);
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
        ChunkPositions::new(ChunksMut {
            chunks: self.chunks.iter_mut().enumerate(),
            released: self.released_chunk.as_ref(),
        })
    }

    pub(crate) fn drain(&mut self) -> SlotDrain<'_, T> {
        self.forget_released_chunks();
        self.released.clear();
        let owned: Owned<T> = |(chunk, entries)| Self::cursor(chunk, take(entries).into_iter());
        ChunkPositions::new(self.chunks.drain(..).enumerate().map(owned))
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

    fn into_iter(mut self) -> Self::IntoIter {
        self.forget_released_chunks();
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

/// Pairs each slot with its position, from the front and from the back.
#[derive(Clone, Debug)]
pub(crate) struct Positions<I> {
    front: usize,
    back: usize,
    slots: I,
}

impl<I> Positions<I> {
    fn new(len: usize, slots: I) -> Positions<I> {
        Positions {
            front: 0,
            back: len,
            slots,
        }
    }
}

impl<I: Iterator> Iterator for Positions<I> {
    type Item = (usize, I::Item);

    fn next(&mut self) -> Option<Self::Item> {
        let slot = self.slots.next()?;
        self.front += 1;
        Some((self.front - 1, slot))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.back - self.front;
        (len, Some(len))
    }
}

impl<I: DoubleEndedIterator> DoubleEndedIterator for Positions<I> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let slot = self.slots.next_back()?;
        self.back -= 1;
        Some((self.back, slot))
    }
}

impl<I: Iterator> ExactSizeIterator for Positions<I> {}

/// The chunks of an arena for a change of their entries. A released chunk gives no entry.
#[derive(Debug)]
pub(crate) struct ChunksMut<'a, T> {
    chunks: iter::Enumerate<slice::IterMut<'a, Chunk<T>>>,
    released: Option<&'a Chunk<T>>,
}

impl<'a, T: Clone> ChunksMut<'a, T> {
    fn cursor(&self, (chunk, entries): (usize, &'a mut Chunk<T>)) -> Cursor<slice::IterMut<'a, Entry<T>>> {
        let entries = Slots::entries_mut(self.released, entries).unwrap_or_default();
        Slots::<T>::cursor(chunk, entries.iter_mut())
    }
}

impl<'a, T: Clone> Iterator for ChunksMut<'a, T> {
    type Item = Cursor<slice::IterMut<'a, Entry<T>>>;

    fn next(&mut self) -> Option<Self::Item> {
        let chunk = self.chunks.next()?;
        Some(self.cursor(chunk))
    }
}

impl<T: Clone> DoubleEndedIterator for ChunksMut<'_, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let chunk = self.chunks.next_back()?;
        Some(self.cursor(chunk))
    }
}

/// The rest of the entries of 1 chunk, with the position of its next front entry.
#[derive(Clone, Debug)]
pub(crate) struct Cursor<I> {
    position: usize,
    entries: I,
}

impl<I: ExactSizeIterator + DoubleEndedIterator> Cursor<I> {
    fn next(&mut self) -> Option<(usize, I::Item)> {
        let entry = self.entries.next()?;
        self.position += 1;
        Some((self.position - 1, entry))
    }

    fn next_back(&mut self) -> Option<(usize, I::Item)> {
        let entry = self.entries.next_back()?;
        Some((self.position + self.entries.len(), entry))
    }
}

/// Pairs each slot of chunks that can give no entry with its position, from the front and from the back. A chunk
/// starts at its index times the chunk length.
#[derive(Debug)]
pub(crate) struct ChunkPositions<C, I> {
    chunks: C,
    front: Option<Cursor<I>>,
    back: Option<Cursor<I>>,
}

impl<C, I> ChunkPositions<C, I> {
    fn new(chunks: C) -> ChunkPositions<C, I> {
        ChunkPositions {
            chunks,
            front: None,
            back: None,
        }
    }
}

impl<C, I> Iterator for ChunkPositions<C, I>
where
    C: Iterator<Item = Cursor<I>>,
    I: ExactSizeIterator + DoubleEndedIterator,
{
    type Item = (usize, I::Item);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(slot) = self.front.as_mut().and_then(Cursor::next) {
                return Some(slot);
            }
            match self.chunks.next() {
                Some(cursor) => self.front = Some(cursor),
                None => return self.back.as_mut()?.next(),
            }
        }
    }
}

impl<C, I> DoubleEndedIterator for ChunkPositions<C, I>
where
    C: DoubleEndedIterator<Item = Cursor<I>>,
    I: ExactSizeIterator + DoubleEndedIterator,
{
    fn next_back(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(slot) = self.back.as_mut().and_then(Cursor::next_back) {
                return Some(slot);
            }
            match self.chunks.next_back() {
                Some(cursor) => self.back = Some(cursor),
                None => return self.front.as_mut()?.next_back(),
            }
        }
    }
}
