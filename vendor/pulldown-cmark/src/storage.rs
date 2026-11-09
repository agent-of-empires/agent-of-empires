use std::convert::Infallible;
use std::ops::Deref;

pub(crate) trait Dictionary<K, V> {
    type Error;

    fn lookup(&self, key: &K) -> Result<Option<V>, Self::Error>;
    fn insert_first(&mut self, key: K, value: V) -> Result<(), Self::Error>;
}

pub(crate) trait Set<K> {
    type Error;

    fn contains(&self, key: &K) -> Result<bool, Self::Error>;
    fn insert(&mut self, key: K) -> Result<(), Self::Error>;
}

pub(crate) trait Sequence<T> {
    type Error;
    type Read<'a>: Deref<Target = T>
    where
        Self: 'a,
        T: 'a;

    fn len(&self) -> usize;
    fn read(&self, index: usize) -> Result<Self::Read<'_>, Self::Error>;
    fn update<R>(
        &mut self,
        index: usize,
        operation: impl FnOnce(&mut T) -> R,
    ) -> Result<R, Self::Error>;
    fn push(&mut self, value: T) -> Result<(), Self::Error>;
    fn pop(&mut self) -> Result<Option<T>, Self::Error>;
    fn truncate(&mut self, length: usize) -> Result<(), Self::Error>;
}

impl<T> Sequence<T> for Vec<T> {
    type Error = Infallible;
    type Read<'a>
        = &'a T
    where
        T: 'a;

    fn len(&self) -> usize {
        Vec::len(self)
    }

    fn read(&self, index: usize) -> Result<&T, Infallible> {
        Ok(&self[index])
    }

    fn update<R>(
        &mut self,
        index: usize,
        operation: impl FnOnce(&mut T) -> R,
    ) -> Result<R, Infallible> {
        Ok(operation(&mut self[index]))
    }

    fn push(&mut self, value: T) -> Result<(), Infallible> {
        Vec::push(self, value);
        Ok(())
    }

    fn pop(&mut self) -> Result<Option<T>, Infallible> {
        Ok(Vec::pop(self))
    }

    fn truncate(&mut self, length: usize) -> Result<(), Infallible> {
        Vec::truncate(self, length);
        Ok(())
    }
}

pub(crate) trait QueueCursor<T: Copy> {
    type Error;

    fn front(&mut self) -> Result<Option<T>, Self::Error>;
    fn pop_front(&mut self) -> Result<(), Self::Error>;
    fn replace_front(&mut self, value: T) -> Result<(), Self::Error>;
}

pub(crate) trait Queues<K, T: Copy> {
    type Error;
    type Cursor<'a>: QueueCursor<T, Error = Self::Error>
    where
        Self: 'a,
        T: 'a;

    fn cursor(&mut self, key: K) -> Result<Self::Cursor<'_>, Self::Error>;
    fn push_back(&mut self, key: K, value: T) -> Result<(), Self::Error>;
    fn clear(&mut self) -> Result<(), Self::Error>;
}

impl<T: Copy> QueueCursor<T> for Option<&mut std::collections::VecDeque<T>> {
    type Error = Infallible;

    fn front(&mut self) -> Result<Option<T>, Infallible> {
        Ok(self.as_ref().and_then(|queue| queue.front().copied()))
    }

    fn pop_front(&mut self) -> Result<(), Infallible> {
        self.as_mut()
            .and_then(|queue| queue.pop_front())
            .expect("queue cursor has no front");
        Ok(())
    }

    fn replace_front(&mut self, value: T) -> Result<(), Infallible> {
        let queue = self.as_deref_mut().expect("queue cursor is empty");
        *queue.front_mut().expect("queue cursor has no front") = value;
        Ok(())
    }
}

impl<K: Eq + std::hash::Hash, T: Copy> Queues<K, T>
    for std::collections::HashMap<K, std::collections::VecDeque<T>>
{
    type Error = Infallible;
    type Cursor<'a>
        = Option<&'a mut std::collections::VecDeque<T>>
    where
        Self: 'a,
        T: 'a;

    fn cursor(&mut self, key: K) -> Result<Self::Cursor<'_>, Infallible> {
        Ok(self.get_mut(&key))
    }

    fn push_back(&mut self, key: K, value: T) -> Result<(), Infallible> {
        self.entry(key).or_default().push_back(value);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Infallible> {
        std::collections::HashMap::clear(self);
        Ok(())
    }
}

#[cfg(feature = "disk")]
pub(crate) struct Owned<T>(pub(crate) T);

#[cfg(feature = "disk")]
impl<T> Deref for Owned<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

#[cfg(feature = "disk")]
impl<T: crate::disk::Record> Sequence<T> for crate::disk::Sequence<'_, T> {
    type Error = std::io::Error;
    type Read<'a>
        = Owned<T>
    where
        Self: 'a,
        T: 'a;

    fn len(&self) -> usize {
        crate::disk::Sequence::len(self) as usize
    }

    fn read(&self, index: usize) -> std::io::Result<Owned<T>> {
        self.get(index as u64).map(Owned)
    }

    fn update<R>(
        &mut self,
        index: usize,
        operation: impl FnOnce(&mut T) -> R,
    ) -> std::io::Result<R> {
        let mut value = self.get(index as u64)?;
        let result = operation(&mut value);
        self.set(index as u64, &value)?;
        Ok(result)
    }

    fn push(&mut self, value: T) -> std::io::Result<()> {
        crate::disk::Sequence::push(self, &value)
    }

    fn pop(&mut self) -> std::io::Result<Option<T>> {
        crate::disk::Sequence::pop(self)
    }

    fn truncate(&mut self, length: usize) -> std::io::Result<()> {
        crate::disk::Sequence::truncate(self, length as u64)
    }
}
