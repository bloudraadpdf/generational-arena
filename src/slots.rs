use super::{vec, Arc, Entry, Vec};
use core::{cmp, iter, mem, ops, slice};

const CHUNK_BYTES: usize = 64 * 1024;

/// The slots of an arena in chunks of about `CHUNK_BYTES`. Each chunk but the last is full, so the arena grows
/// without a copy of its slots and keeps less than one chunk of spare slots. A clone shares each chunk until one of
/// the two changes it.
#[derive(Clone, Debug)]
pub(crate) struct Slots<T> {
    chunks: Vec<Chunk<T>>,
}

type Chunk<T> = Arc<[Entry<T>]>;
type Shared<'a, T> = fn(&'a Chunk<T>) -> &'a [Entry<T>];
type Unique<'a, T> = fn(&'a mut Chunk<T>) -> &'a mut [Entry<T>];
type Take<T> = fn(Chunk<T>) -> Vec<Entry<T>>;

pub(crate) type SlotIter<'a, T> = Positions<iter::Flatten<iter::Map<slice::Iter<'a, Chunk<T>>, Shared<'a, T>>>>;
pub(crate) type SlotIterMut<'a, T> = Positions<iter::Flatten<iter::Map<slice::IterMut<'a, Chunk<T>>, Unique<'a, T>>>>;
pub(crate) type SlotDrain<'a, T> = Positions<iter::Flatten<iter::Map<vec::Drain<'a, Chunk<T>>, Take<T>>>>;
pub(crate) type SlotIntoIter<T> = iter::Flatten<iter::Map<vec::IntoIter<Chunk<T>>, Take<T>>>;

impl<T> Slots<T> {
    const CHUNK_LEN: usize = CHUNK_BYTES.div_ceil(mem::size_of::<Entry<T>>());

    pub(crate) fn new() -> Slots<T> {
        Slots { chunks: Vec::new() }
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

    pub(crate) fn get(&self, slot: usize) -> Option<&Entry<T>> {
        let (chunk, offset) = Self::position(slot);
        self.chunks.get(chunk)?.get(offset)
    }

    pub(crate) fn iter(&self) -> SlotIter<'_, T> {
        let shared: Shared<'_, T> = AsRef::as_ref;
        Positions::new(self.len(), self.chunks.iter().map(shared).flatten())
    }
}

impl<T: Clone> Slots<T> {
    pub(crate) fn get_mut(&mut self, slot: usize) -> Option<&mut Entry<T>> {
        let (chunk, offset) = Self::position(slot);
        Arc::make_mut(self.chunks.get_mut(chunk)?).get_mut(offset)
    }

    /// Panics if `a` and `b` are the same slot or either is out of range.
    pub(crate) fn pair_mut(&mut self, a: usize, b: usize) -> [&mut Entry<T>; 2] {
        let ((chunk_a, offset_a), (chunk_b, offset_b)) = (Self::position(a), Self::position(b));
        if chunk_a == chunk_b {
            Arc::make_mut(&mut self.chunks[chunk_a])
                .get_disjoint_mut([offset_a, offset_b])
                .expect("two distinct slots")
        } else {
            let [a, b] = self
                .chunks
                .get_disjoint_mut([chunk_a, chunk_b])
                .expect("two distinct chunks");
            [&mut Arc::make_mut(a)[offset_a], &mut Arc::make_mut(b)[offset_b]]
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
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        let (full, rest) = Self::position(len);
        self.chunks.truncate(full + usize::from(rest > 0));
        if self.chunks.get(full).is_some_and(|last| last.len() > rest) {
            let mut last = self.chunks.pop().map_or_else(Vec::new, take);
            last.truncate(rest);
            self.chunks.push(last.into());
        }
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.chunks.shrink_to_fit();
    }

    pub(crate) fn iter_mut(&mut self) -> SlotIterMut<'_, T> {
        let unique: Unique<'_, T> = Arc::make_mut;
        Positions::new(self.len(), self.chunks.iter_mut().map(unique).flatten())
    }

    pub(crate) fn drain(&mut self) -> SlotDrain<'_, T> {
        Positions::new(self.len(), self.chunks.drain(..).map(take as Take<T>).flatten())
    }
}

/// The entries of `chunk`, without a copy if no other arena shares it.
fn take<T: Clone>(mut chunk: Chunk<T>) -> Vec<Entry<T>> {
    Arc::make_mut(&mut chunk)
        .iter_mut()
        .map(|entry| mem::replace(entry, Entry::Free { next_free: None }))
        .collect()
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
