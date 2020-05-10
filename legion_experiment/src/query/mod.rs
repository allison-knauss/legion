use crate::{
    iter::indexed::TrustedRandomAccess,
    storage::{
        archetype::{Archetype, ArchetypeIndex},
        component::Component,
        group::SubGroup,
    },
    world::{World, WorldId},
};
use filter::{DynamicFilter, EntityFilter, GroupMatcher};
use parking_lot::Mutex;
use std::{collections::HashMap, marker::PhantomData, ops::Range, slice::Iter};
use view::{Fetch, IntoIndexableIter, ReadOnlyFetch, View};

pub mod filter;
pub mod view;

pub trait IntoQuery: for<'a> View<'a> {
    fn query() -> Query<Self, Self::Filter>;
}

impl<T: for<'a> View<'a>> IntoQuery for T {
    fn query() -> Query<Self, Self::Filter> {
        Self::validate();

        Query {
            _view: PhantomData,
            filter: Mutex::new(<Self::Filter as Default>::default()),
            layout_matches: HashMap::new(),
        }
    }
}

/// Contains the result of an entity layout filter.
#[derive(Debug, Clone)]
pub struct QueryResult<'a> {
    index: &'a [ArchetypeIndex],
    range: Range<usize>,
    is_ordered: bool,
}

impl<'a> QueryResult<'a> {
    fn unordered(index: &'a [ArchetypeIndex]) -> Self {
        Self {
            range: 0..index.len(),
            index,
            is_ordered: false,
        }
    }

    fn ordered(index: &'a [ArchetypeIndex]) -> Self {
        Self {
            range: 0..index.len(),
            index,
            is_ordered: true,
        }
    }

    pub fn into_index(self) -> &'a [ArchetypeIndex] { &self.index[self.range] }

    pub fn index(&self) -> &[ArchetypeIndex] { &self.index[self.range.clone()] }

    pub fn range(&self) -> &Range<usize> { &self.range }

    pub fn is_ordered(&self) -> bool { self.is_ordered }

    pub fn len(&self) -> usize { self.range.len() }

    pub fn is_empty(&self) -> bool { self.index().is_empty() }

    pub fn split_at(self, index: usize) -> (Self, Self) {
        (
            Self {
                range: self.range.start..index,
                index: self.index,
                is_ordered: self.is_ordered,
            },
            Self {
                range: index..self.range.end,
                index: self.index,
                is_ordered: self.is_ordered,
            },
        )
    }
}

#[derive(Debug, Clone)]
enum Cache {
    Unordered {
        archetypes: Vec<ArchetypeIndex>,
        seen: usize,
    },
    Ordered {
        group: usize,
        subgroup: SubGroup,
    },
}

pub struct Query<V: for<'a> View<'a>, F: EntityFilter> {
    _view: PhantomData<V>,
    filter: Mutex<F>,
    layout_matches: HashMap<WorldId, Cache>,
}

impl<V: for<'a> View<'a>, F: EntityFilter> Query<V, F> {
    /// Adds an additional filter to the query.
    pub fn filter<T: EntityFilter>(self, filter: T) -> Query<V, <F as std::ops::BitAnd<T>>::Output>
    where
        F: std::ops::BitAnd<T>,
        <F as std::ops::BitAnd<T>>::Output: EntityFilter,
    {
        Query {
            _view: self._view,
            filter: Mutex::new(self.filter.into_inner() & filter),
            layout_matches: HashMap::default(),
        }
    }

    #[inline]
    pub unsafe fn iter_unchecked<'a>(
        &'a mut self,
        world: &'a World,
    ) -> std::iter::Flatten<ChunkIter<'a, 'a, V, F>> {
        self.iter_chunks_unchecked(world).flatten()
    }

    #[cfg(feature = "par-iter")]
    #[inline]
    pub unsafe fn par_iter_unchecked<'a>(
        &'a mut self,
        world: &'a World,
    ) -> rayon::iter::Flatten<par_iter::ParChunkIter<'a, V, F>> {
        use rayon::iter::ParallelIterator;
        self.par_iter_chunks_unchecked(world).flatten()
    }

    #[inline]
    pub fn iter_mut<'a>(
        &'a mut self,
        world: &'a mut World,
    ) -> std::iter::Flatten<ChunkIter<'a, 'a, V, F>> {
        // safety: we have exclusive access to world
        unsafe { self.iter_unchecked(world) }
    }

    #[cfg(feature = "par-iter")]
    #[inline]
    pub fn par_iter_mut<'a>(
        &'a mut self,
        world: &'a mut World,
    ) -> rayon::iter::Flatten<par_iter::ParChunkIter<'a, V, F>> {
        // safety: we have exclusive access to world
        unsafe { self.par_iter_unchecked(world) }
    }

    #[inline]
    pub fn iter<'a>(&'a mut self, world: &'a World) -> std::iter::Flatten<ChunkIter<'a, 'a, V, F>>
    where
        <V as View<'a>>::Fetch: ReadOnlyFetch,
    {
        // safety: the view is readonly - it cannot create mutable aliases
        unsafe { self.iter_unchecked(world) }
    }

    #[cfg(feature = "par-iter")]
    #[inline]
    pub fn par_iter<'a>(
        &'a mut self,
        world: &'a World,
    ) -> rayon::iter::Flatten<par_iter::ParChunkIter<'a, V, F>>
    where
        <V as View<'a>>::Fetch: ReadOnlyFetch,
    {
        // safety: the view is readonly - it cannot create mutable aliases
        unsafe { self.par_iter_unchecked(world) }
    }

    fn evaluate_query<'a>(&'a mut self, world: &'a World) -> (&mut Mutex<F>, QueryResult<'a>) {
        let cache = self.layout_matches.entry(world.id()).or_insert_with(|| {
            let cache = if F::can_match_group() {
                let components = F::group_components();
                components
                    .iter()
                    .next()
                    .and_then(|t| world.group(*t))
                    .map(|(i, g)| (i, g.exact_match(&components)))
                    .and_then(|(group, subgroup)| {
                        subgroup.map(|subgroup| Cache::Ordered { group, subgroup })
                    })
            } else {
                None
            };

            cache.unwrap_or_else(|| Cache::Unordered {
                archetypes: Vec::new(),
                seen: 0,
            })
        });

        let filter = self.filter.get_mut();
        let result = match cache {
            Cache::Unordered { archetypes, seen } => {
                for archetype in world.layout_index().search_from(&*filter, *seen) {
                    archetypes.push(archetype);
                }
                *seen = world.archetypes().len();
                QueryResult::unordered(archetypes.as_slice())
            }
            Cache::Ordered { group, subgroup } => {
                let archetypes = &world.groups()[*group][*subgroup];
                QueryResult::ordered(archetypes)
            }
        };

        (&mut self.filter, result)
    }

    pub unsafe fn iter_chunks_unchecked<'a>(
        &'a mut self,
        world: &'a World,
    ) -> ChunkIter<'a, 'a, V, F> {
        let (filter, result) = self.evaluate_query(world);
        let indices = result.index.iter();
        let fetch = <V as View<'a>>::fetch(world.components(), world.archetypes(), result);
        ChunkIter {
            inner: fetch,
            filter: filter.get_mut(),
            archetypes: world.archetypes(),
            max_count: indices.len(),
            indices,
        }
    }

    #[cfg(feature = "par-iter")]
    pub unsafe fn par_iter_chunks_unchecked<'a>(
        &'a mut self,
        world: &'a World,
    ) -> par_iter::ParChunkIter<'a, V, F> {
        let (filter, result) = self.evaluate_query(world);
        par_iter::ParChunkIter::new(world, result, filter)
    }

    #[inline]
    pub fn iter_chunks_mut<'a>(&'a mut self, world: &'a mut World) -> ChunkIter<'a, 'a, V, F> {
        // safety: we have exclusive access to world
        unsafe { self.iter_chunks_unchecked(world) }
    }

    #[cfg(feature = "par-iter")]
    #[inline]
    pub fn par_iter_chunks_mut<'a>(
        &'a mut self,
        world: &'a mut World,
    ) -> par_iter::ParChunkIter<'a, V, F> {
        // safety: we have exclusive access to world
        unsafe { self.par_iter_chunks_unchecked(world) }
    }

    #[inline]
    pub fn iter_chunks<'a>(&'a mut self, world: &'a World) -> ChunkIter<'a, 'a, V, F>
    where
        <V as View<'a>>::Fetch: ReadOnlyFetch,
    {
        // safety: the view is readonly - it cannot create mutable aliases
        unsafe { self.iter_chunks_unchecked(world) }
    }

    #[cfg(feature = "par-iter")]
    #[inline]
    pub fn par_iter_chunks<'a>(&'a mut self, world: &'a World) -> par_iter::ParChunkIter<'a, V, F>
    where
        <V as View<'a>>::Fetch: ReadOnlyFetch,
    {
        // safety: the view is readonly - it cannot create mutable aliases
        unsafe { self.par_iter_chunks_unchecked(world) }
    }

    #[inline]
    pub unsafe fn for_each_unchecked<'a, Body>(&'a mut self, world: &'a World, mut f: Body)
    where
        Body: FnMut(<V as View<'a>>::Element),
    {
        // we use a nested loop because it is significantly faster than .flatten()
        for chunk in self.iter_chunks_unchecked(world) {
            for entities in chunk {
                f(entities);
            }
        }
    }

    #[cfg(feature = "par-iter")]
    #[inline]
    pub unsafe fn par_for_each_unchecked<'a, Body>(&'a mut self, world: &'a World, f: Body)
    where
        Body: Fn(<V as View<'a>>::Element) + Send + Sync,
    {
        use rayon::iter::{IntoParallelIterator, ParallelIterator};
        self.par_iter_chunks_unchecked(world)
            .for_each(move |chunk| chunk.into_par_iter().for_each(&f));
    }

    #[inline]
    pub fn for_each_mut<'a, Body>(&'a mut self, world: &'a mut World, f: Body)
    where
        Body: FnMut(<V as View<'a>>::Element),
    {
        // safety: we have exclusive access to world
        unsafe { self.for_each_unchecked(world, f) };
    }

    #[cfg(feature = "par-iter")]
    #[inline]
    pub fn par_for_each_mut<'a, Body>(&'a mut self, world: &'a mut World, f: Body)
    where
        Body: Fn(<V as View<'a>>::Element) + Send + Sync,
    {
        // safety: we have exclusive access to world
        unsafe { self.par_for_each_unchecked(world, f) };
    }

    #[inline]
    pub fn for_each<'a, Body>(&'a mut self, world: &'a World, f: Body)
    where
        Body: FnMut(<V as View<'a>>::Element),
        <V as View<'a>>::Fetch: ReadOnlyFetch,
    {
        // safety: the view is readonly - it cannot create mutable aliases
        unsafe { self.for_each_unchecked(world, f) };
    }

    #[cfg(feature = "par-iter")]
    #[inline]
    pub fn par_for_each<'a, Body>(&'a mut self, world: &'a World, f: Body)
    where
        Body: Fn(<V as View<'a>>::Element) + Send + Sync,
        <V as View<'a>>::Fetch: ReadOnlyFetch,
    {
        // safety: the view is readonly - it cannot create mutable aliases
        unsafe { self.par_for_each_unchecked(world, f) };
    }
}

pub struct ChunkView<'a, F: Fetch> {
    archetype: &'a Archetype,
    fetch: F,
}

impl<'a, F: Fetch> ChunkView<'a, F> {
    fn new(archetype: &'a Archetype, fetch: F) -> Self { Self { archetype, fetch } }

    pub fn archetype(&self) -> &Archetype { &self.archetype }

    pub fn component_slice<T: Component>(&self) -> Option<&[T]> { self.fetch.find::<T>() }

    pub fn component_slice_mut<T: Component>(&mut self) -> Option<&mut [T]> {
        self.fetch.find_mut::<T>()
    }

    pub fn into_components(self) -> F::Data { self.fetch.into_components() }

    pub fn get_components(&self) -> F::Data
    where
        F: ReadOnlyFetch,
    {
        self.fetch.get_components()
    }
}

impl<'a, F: Fetch> IntoIterator for ChunkView<'a, F> {
    type IntoIter = <F as IntoIndexableIter>::IntoIter;
    type Item = <F as IntoIndexableIter>::Item;
    fn into_iter(self) -> Self::IntoIter { self.fetch.into_indexable_iter() }
}

#[cfg(feature = "par-iter")]
impl<'a, F: Fetch> rayon::iter::IntoParallelIterator for ChunkView<'a, F> {
    type Iter = crate::iter::indexed::par_iter::Par<<F as IntoIndexableIter>::IntoIter>;
    type Item = <<F as IntoIndexableIter>::IntoIter as TrustedRandomAccess>::Item;
    fn into_par_iter(self) -> Self::Iter {
        use crate::iter::indexed::par_iter::Par;
        Par::new(self.fetch.into_indexable_iter())
    }
}

pub struct ChunkIter<'world, 'query, V, D>
where
    V: View<'world>,
    D: DynamicFilter + 'query,
{
    inner: V::Iter,
    indices: Iter<'query, ArchetypeIndex>,
    filter: &'query mut D,
    archetypes: &'world [Archetype],
    max_count: usize,
}

impl<'world, 'query, V, D> Iterator for ChunkIter<'world, 'query, V, D>
where
    V: View<'world>,
    D: DynamicFilter + 'query,
{
    type Item = ChunkView<'world, V::Fetch>;

    fn next(&mut self) -> Option<Self::Item> {
        for mut fetch in &mut self.inner {
            let idx = self.indices.next().unwrap();
            if self.filter.matches_archetype(&fetch).is_pass() {
                fetch.accepted();
                return Some(ChunkView::new(&self.archetypes[*idx], fetch));
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) { (0, Some(self.max_count)) }
}

// impl<'world, 'query, I, F> Iterator for ChunkIter<'world, 'query, I, Passthrough, F>
// where
//     I: Iterator<Item = (ArchetypeIndex, F)>,
//     F: Fetch,
// {
//     type Item = ChunkView<'world, F>;

//     fn next(&mut self) -> Option<Self::Item> {
//         for (index, mut fetch) in &mut self.inner {
//             fetch.accepted();
//             return Some(ChunkView::new(&self.archetypes[index], fetch));
//         }
//         None
//     }

//     fn size_hint(&self) -> (usize, Option<usize>) { (self.max_count, Some(self.max_count)) }
// }

// impl<'world, 'query, I, F> ExactSizeIterator for ChunkIter<'world, 'query, I, Passthrough, F>
// where
//     I: Iterator<Item = (ArchetypeIndex, F)> + ExactSizeIterator,
//     F: Fetch,
// {
// }

#[cfg(feature = "par-iter")]
mod par_iter {
    use super::*;
    use crate::world::World;
    use rayon::iter::plumbing::{bridge_unindexed, Folder, UnindexedConsumer, UnindexedProducer};
    use rayon::iter::ParallelIterator;
    use std::marker::PhantomData;

    pub struct Iter<'world, 'query, V, D>
    where
        V: View<'world>,
        D: DynamicFilter + 'query,
    {
        inner: V::Iter,
        indices: std::slice::Iter<'query, ArchetypeIndex>,
        filter: &'query Mutex<D>,
        archetypes: &'world [Archetype],
        max_count: usize,
    }

    impl<'world, 'query, V, D> Iterator for Iter<'world, 'query, V, D>
    where
        V: View<'world>,
        D: DynamicFilter + 'query,
    {
        type Item = ChunkView<'world, V::Fetch>;

        fn next(&mut self) -> Option<Self::Item> {
            let mut filter = self.filter.lock();
            for mut fetch in &mut self.inner {
                let idx = self.indices.next().unwrap();
                if filter.matches_archetype(&fetch).is_pass() {
                    fetch.accepted();
                    return Some(ChunkView::new(&self.archetypes[*idx], fetch));
                }
            }
            None
        }

        fn size_hint(&self) -> (usize, Option<usize>) { (0, Some(self.max_count)) }
    }

    pub struct ParChunkIter<'a, V, D>
    where
        V: View<'a>,
        D: DynamicFilter + 'a,
    {
        world: &'a World,
        result: QueryResult<'a>,
        filter: &'a Mutex<D>,
        _view: PhantomData<V>,
    }

    impl<'a, V, D> ParChunkIter<'a, V, D>
    where
        V: View<'a>,
        D: DynamicFilter + 'a,
    {
        pub fn new(world: &'a World, result: QueryResult<'a>, filter: &'a Mutex<D>) -> Self {
            Self {
                world,
                result,
                filter,
                _view: PhantomData,
            }
        }
    }

    unsafe impl<'a, V, D> Send for ParChunkIter<'a, V, D>
    where
        V: View<'a>,
        D: DynamicFilter + 'a,
    {
    }

    unsafe impl<'a, V, D> Sync for ParChunkIter<'a, V, D>
    where
        V: View<'a>,
        D: DynamicFilter + 'a,
    {
    }

    impl<'a, V, D> UnindexedProducer for ParChunkIter<'a, V, D>
    where
        V: View<'a>,
        D: DynamicFilter + 'a,
    {
        type Item = <Iter<'a, 'a, V, D> as Iterator>::Item;

        fn split(self) -> (Self, Option<Self>) {
            let index = self.result.len() / 2;
            let (left, right) = self.result.split_at(index);
            (
                Self {
                    world: self.world,
                    result: right,
                    filter: self.filter,
                    _view: PhantomData,
                },
                if left.len() > 0 {
                    Some(Self {
                        world: self.world,
                        result: left,
                        filter: self.filter,
                        _view: PhantomData,
                    })
                } else {
                    None
                },
            )
        }

        fn fold_with<F>(self, folder: F) -> F
        where
            F: Folder<Self::Item>,
        {
            let indices = self.result.index.iter();
            let fetch = unsafe {
                <V as View<'a>>::fetch(
                    self.world.components(),
                    self.world.archetypes(),
                    self.result,
                )
            };
            let iter = Iter::<'a, 'a, V, D> {
                inner: fetch,
                filter: self.filter,
                archetypes: self.world.archetypes(),
                max_count: indices.len(),
                indices,
            };
            folder.consume_iter(iter)
        }
    }

    impl<'a, V, D> ParallelIterator for ParChunkIter<'a, V, D>
    where
        V: View<'a>,
        D: DynamicFilter + 'a,
    {
        type Item = ChunkView<'a, V::Fetch>;

        fn drive_unindexed<C>(self, consumer: C) -> C::Result
        where
            C: UnindexedConsumer<Self::Item>,
        {
            bridge_unindexed(self, consumer)
        }
    }
}

#[cfg(test)]
mod test {
    use super::view::{read::Read, write::Write};
    use super::IntoQuery;
    use crate::world::World;

    #[test]
    fn query() {
        let mut world = World::default();
        world.extend(vec![(1usize, true), (2usize, true), (3usize, false)]);

        let mut query = <(Read<usize>, Write<bool>)>::query();
        for (x, y) in query.iter_mut(&mut world) {
            println!("{}, {}", x, y);
        }
        for chunk in query.iter_chunks_mut(&mut world) {
            let (x, y) = chunk.into_components();
            println!("{:?}, {:?}", x, y);
        }
        println!("parallel");
        query.par_for_each_mut(&mut world, |(x, y)| println!("{:?}, {:?}", x, y));
        println!("par chunks");
        use rayon::iter::ParallelIterator;
        query.par_iter_chunks_mut(&mut world).for_each(|chunk| {
            println!("arch {:?}", chunk.archetype());
            let (x, y) = chunk.into_components();
            println!("{:?}, {:?}", x, y);
        })
    }
}
