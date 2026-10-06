use super::{Arena, Entry, Generation, Index, Slots, Vec, DEFAULT_CAPACITY};
use core::cmp;
use core::fmt;
use core::marker::PhantomData;
use serde::de::{Deserialize, Deserializer, Error, SeqAccess, Visitor};
use serde::ser::{Serialize, Serializer};

impl Serialize for Index {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Note: do not change the serialization format, or it may break
        // forward and backward compatibility of serialized data!
        self.into_raw_parts().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Index {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let (index, generation) = Deserialize::deserialize(deserializer)?;
        Index::try_from_raw_parts(index, generation)
            .ok_or_else(|| D::Error::custom("the raw parts of an index exceed 32 bits"))
    }
}

impl<T> Serialize for Arena<T>
where
    T: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Note: do not change the serialization format, or it may break
        // forward and backward compatibility of serialized data!
        serializer.collect_seq((0..self.items.len()).map(|slot| match self.items.get(slot) {
            Some(Entry::Occupied { generation, value }) => Some((generation.value(), value)),
            _ => None,
        }))
    }
}

impl<'de, T> Deserialize<'de> for Arena<T>
where
    T: Deserialize<'de> + Clone,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(ArenaVisitor::new())
    }
}

struct ArenaVisitor<T> {
    marker: PhantomData<fn() -> Arena<T>>,
}

impl<T> ArenaVisitor<T> {
    fn new() -> Self {
        Self {
            marker: PhantomData,
        }
    }
}

impl<'de, T> Visitor<'de> for ArenaVisitor<T>
where
    T: Deserialize<'de> + Clone,
{
    type Value = Arena<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "a generational arena")
    }

    fn visit_seq<M>(self, mut access: M) -> Result<Self::Value, M::Error>
    where
        M: SeqAccess<'de>,
    {
        let init_cap = access.size_hint().unwrap_or(DEFAULT_CAPACITY);
        let mut entries = Vec::with_capacity(init_cap);

        let mut generation = Generation::FIRST;
        let mut len = 0;
        while let Some(element) = access.next_element::<Option<(u64, T)>>()? {
            let entry = match element {
                Some((gen, value)) => {
                    let gen = Generation::from_value(gen)
                        .ok_or_else(|| M::Error::custom("an arena generation exceeds 32 bits"))?;
                    generation = cmp::max(generation, gen);
                    len += 1;
                    Entry::Occupied {
                        generation: gen,
                        value,
                    }
                }
                None => Entry::Free { next_free: None },
            };
            entries.push(entry);
        }

        let mut items = Slots::new();
        items.extend(entries.into_iter());
        let mut arena = Arena {
            items,
            generation,
            free_list_head: None,
            len,
        };
        arena.relink_free_list();
        Ok(arena)
    }
}
