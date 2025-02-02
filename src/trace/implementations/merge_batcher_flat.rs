//! A general purpose `Batcher` implementation for FlatStack.

use std::cmp::Ordering;
use std::marker::PhantomData;
use timely::progress::frontier::{Antichain, AntichainRef};
use timely::{Data, PartialOrder};
use timely::container::flatcontainer::{Push, FlatStack, Region, ReserveItems};
use timely::container::flatcontainer::impls::tuple::TupleABCRegion;

use crate::difference::{IsZero, Semigroup};
use crate::trace::implementations::merge_batcher::Merger;
use crate::trace::cursor::IntoOwned;

/// A merger for flat stacks.
///
/// `MC` is a [`Region`] that implements [`MergerChunk`].
pub struct FlatcontainerMerger<MC> {
    _marker: PhantomData<MC>,
}

impl<MC> Default for FlatcontainerMerger<MC> {
    fn default() -> Self {
        Self { _marker: PhantomData, }
    }
}

impl<MC: Region> FlatcontainerMerger<MC> {
    const BUFFER_SIZE_BYTES: usize = 8 << 10;
    fn chunk_capacity(&self) -> usize {
        let size = ::std::mem::size_of::<MC::Index>();
        if size == 0 {
            Self::BUFFER_SIZE_BYTES
        } else if size <= Self::BUFFER_SIZE_BYTES {
            Self::BUFFER_SIZE_BYTES / size
        } else {
            1
        }
    }

    /// Helper to get pre-sized vector from the stash.
    #[inline]
    fn empty(&self, stash: &mut Vec<FlatStack<MC>>) -> FlatStack<MC> {
        stash.pop().unwrap_or_else(|| FlatStack::with_capacity(self.chunk_capacity()))
    }

    /// Helper to return a chunk to the stash.
    #[inline]
    fn recycle(&self, mut chunk: FlatStack<MC>, stash: &mut Vec<FlatStack<MC>>) {
        // TODO: Should we limit the size of `stash`?
        if chunk.capacity() == self.chunk_capacity() {
            chunk.clear();
            stash.push(chunk);
        }
    }
}

/// Behavior to dissect items of chunks in the merge batcher
pub trait MergerChunk: Region {
    /// The data portion of the update
    type Data<'a>: Ord where Self: 'a;
    /// The time of the update
    type Time<'a>: Ord where Self: 'a;
    /// The owned time type.
    type TimeOwned;
    /// The diff of the update
    type Diff<'a> where Self: 'a;
    /// The owned diff type.
    type DiffOwned;

    /// Split a read item into its constituents. Must be cheap.
    fn into_parts<'a>(item: Self::ReadItem<'a>) -> (Self::Data<'a>, Self::Time<'a>, Self::Diff<'a>);
}

impl<D,T,R> MergerChunk for TupleABCRegion<D, T, R>
where
    D: Region,
    for<'a> D::ReadItem<'a>: Ord,
    T: Region,
    for<'a> T::ReadItem<'a>: Ord,
    R: Region,
{
    type Data<'a> = D::ReadItem<'a> where Self: 'a;
    type Time<'a> = T::ReadItem<'a> where Self: 'a;
    type TimeOwned = T::Owned;
    type Diff<'a> = R::ReadItem<'a> where Self: 'a;
    type DiffOwned = R::Owned;

    fn into_parts<'a>((data, time, diff): Self::ReadItem<'a>) -> (Self::Data<'a>, Self::Time<'a>, Self::Diff<'a>) {
        (data, time, diff)
    }
}

impl<MC> Merger for FlatcontainerMerger<MC>
where
    for<'a> MC: MergerChunk + Clone + 'static
        + ReserveItems<<MC as Region>::ReadItem<'a>>
        + Push<<MC as Region>::ReadItem<'a>>
        + Push<(MC::Data<'a>, MC::Time<'a>, &'a MC::DiffOwned)>
        + Push<(MC::Data<'a>, MC::Time<'a>, MC::Diff<'a>)>,
    for<'a> MC::Time<'a>: PartialOrder<MC::TimeOwned> + Copy + IntoOwned<'a, Owned=MC::TimeOwned>,
    for<'a> MC::Diff<'a>: IntoOwned<'a, Owned = MC::DiffOwned>,
    for<'a> MC::TimeOwned: Ord + PartialOrder + PartialOrder<MC::Time<'a>> + Data,
    for<'a> MC::DiffOwned: Default + Semigroup + Semigroup<MC::Diff<'a>> + Data,
{
    type Time = MC::TimeOwned;
    type Chunk = FlatStack<MC>;

    fn merge(&mut self, list1: Vec<Self::Chunk>, list2: Vec<Self::Chunk>, output: &mut Vec<Self::Chunk>, stash: &mut Vec<Self::Chunk>) {
        let mut list1 = list1.into_iter();
        let mut list2 = list2.into_iter();

        let mut head1 = <FlatStackQueue<MC>>::from(list1.next().unwrap_or_default());
        let mut head2 = <FlatStackQueue<MC>>::from(list2.next().unwrap_or_default());

        let mut result = self.empty(stash);

        let mut diff = MC::DiffOwned::default();

        // while we have valid data in each input, merge.
        while !head1.is_empty() && !head2.is_empty() {
            while (result.capacity() - result.len()) > 0 && !head1.is_empty() && !head2.is_empty() {
                let cmp = {
                    let (data1, time1, _diff) = MC::into_parts(head1.peek());
                    let (data2, time2, _diff) = MC::into_parts(head2.peek());
                    (data1, time1).cmp(&(data2, time2))
                };
                // TODO: The following less/greater branches could plausibly be a good moment for
                // `copy_range`, on account of runs of records that might benefit more from a
                // `memcpy`.
                match cmp {
                    Ordering::Less => {
                        result.copy(head1.pop());
                    }
                    Ordering::Greater => {
                        result.copy(head2.pop());
                    }
                    Ordering::Equal => {
                        let (data, time1, diff1) = MC::into_parts(head1.pop());
                        let (_data, _time2, diff2) = MC::into_parts(head2.pop());
                        diff1.clone_onto(&mut diff);
                        diff.plus_equals(&diff2);
                        if !diff.is_zero() {
                            result.copy((data, time1, &diff));
                        }
                    }
                }
            }

            if result.capacity() == result.len() {
                output.push(result);
                result = self.empty(stash);
            }

            if head1.is_empty() {
                self.recycle(head1.done(), stash);
                head1 = FlatStackQueue::from(list1.next().unwrap_or_default());
            }
            if head2.is_empty() {
                self.recycle(head2.done(), stash);
                head2 = FlatStackQueue::from(list2.next().unwrap_or_default());
            }
        }

        while !head1.is_empty() {
            let advance = result.capacity() - result.len();
            let iter = head1.iter().take(advance);
            result.reserve_items(iter.clone());
            for item in iter {
                result.copy(item);
            }
            output.push(result);
            head1.advance(advance);
            result = self.empty(stash);
        }
        if !result.is_empty() {
            output.push(result);
            result = self.empty(stash);
        }
        output.extend(list1);
        self.recycle(head1.done(), stash);

        while !head2.is_empty() {
            let advance = result.capacity() - result.len();
            let iter = head2.iter().take(advance);
            result.reserve_items(iter.clone());
            for item in iter {
                result.copy(item);
            }
            output.push(result);
            head2.advance(advance);
            result = self.empty(stash);
        }
        output.extend(list2);
        self.recycle(head2.done(), stash);
    }

    fn extract(
        &mut self,
        merged: Vec<Self::Chunk>,
        upper: AntichainRef<Self::Time>,
        frontier: &mut Antichain<Self::Time>,
        readied: &mut Vec<Self::Chunk>,
        kept: &mut Vec<Self::Chunk>,
        stash: &mut Vec<Self::Chunk>,
    ) {
        let mut keep = self.empty(stash);
        let mut ready = self.empty(stash);

        for buffer in merged {
            for (data, time, diff) in buffer.iter().map(MC::into_parts) {
                if upper.less_equal(&time) {
                    frontier.insert_with(&time, |time| (*time).into_owned());
                    if keep.len() == keep.capacity() && !keep.is_empty() {
                        kept.push(keep);
                        keep = self.empty(stash);
                    }
                    keep.copy((data, time, diff));
                } else {
                    if ready.len() == ready.capacity() && !ready.is_empty() {
                        readied.push(ready);
                        ready = self.empty(stash);
                    }
                    ready.copy((data, time, diff));
                }
            }
            // Recycling buffer.
            self.recycle(buffer, stash);
        }
        // Finish the kept data.
        if !keep.is_empty() {
            kept.push(keep);
        }
        if !ready.is_empty() {
            readied.push(ready);
        }
    }

    fn account(chunk: &Self::Chunk) -> (usize, usize, usize, usize) {
        let (mut size, mut capacity, mut allocations) = (0, 0, 0);
        let cb = |siz, cap| {
            size += siz;
            capacity += cap;
            allocations += 1;
        };
        chunk.heap_size(cb);
        (chunk.len(), size, capacity, allocations)
    }
}

struct FlatStackQueue<R: Region> {
    list: FlatStack<R>,
    head: usize,
}

impl<R: Region> Default for FlatStackQueue<R> {
    fn default() -> Self {
        Self::from(Default::default())
    }
}

impl<R: Region> FlatStackQueue<R> {
    fn pop(&mut self) -> R::ReadItem<'_> {
        self.head += 1;
        self.list.get(self.head - 1)
    }

    fn peek(&self) -> R::ReadItem<'_> {
        self.list.get(self.head)
    }

    fn from(list: FlatStack<R>) -> Self {
        FlatStackQueue { list, head: 0 }
    }

    fn done(self) -> FlatStack<R> {
        self.list
    }

    fn is_empty(&self) -> bool {
        self.head >= self.list.len()
    }

    /// Return an iterator over the remaining elements.
    fn iter(&self) -> impl Iterator<Item = R::ReadItem<'_>> + Clone {
        self.list.iter().skip(self.head)
    }

    fn advance(&mut self, consumed: usize) {
        self.head += consumed;
    }
}
