extern crate generational_arena;
use generational_arena::{Arena, Index};

#[test]
fn an_optional_index_takes_8_bytes() {
    assert_eq!(core::mem::size_of::<Index>(), 8);
    assert_eq!(core::mem::size_of::<Option<Index>>(), 8);
}

#[test]
fn raw_parts_round_trip_the_generation_of_a_reused_slot() {
    let mut arena = Arena::new();
    let first = arena.insert(1);
    arena.remove(first);
    let second = arena.insert(2);
    assert_eq!(first.into_raw_parts(), (0, 0));
    assert_eq!(second.into_raw_parts(), (0, 1));
    let (index, generation) = second.into_raw_parts();
    assert_eq!(Index::from_raw_parts(index, generation), second);
}
