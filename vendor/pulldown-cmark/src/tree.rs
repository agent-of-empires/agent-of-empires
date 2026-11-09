// Copyright 2018 Google LLC
//
// Use of this source code is governed by an MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

//! A preorder tree with independent node and traversal storage.

use std::num::NonZeroUsize;
use std::ops::{Add, Sub};

use crate::parse::{Item, ItemBody, ListFlags};
use crate::storage::Sequence;

#[cfg(feature = "disk")]
mod record;

#[derive(Debug, Eq, PartialEq, Copy, Clone, PartialOrd)]
pub(crate) struct TreeIndex(NonZeroUsize);

impl TreeIndex {
    fn new(i: usize) -> Self {
        TreeIndex(NonZeroUsize::new(i).unwrap())
    }

    pub fn get(self) -> usize {
        self.0.get()
    }
}

impl Add<usize> for TreeIndex {
    type Output = TreeIndex;

    fn add(self, rhs: usize) -> Self {
        let inner = self.0.get() + rhs;
        TreeIndex::new(inner)
    }
}

impl Sub<usize> for TreeIndex {
    type Output = TreeIndex;

    fn sub(self, rhs: usize) -> Self {
        let inner = self.0.get().checked_sub(rhs).unwrap();
        TreeIndex::new(inner)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Node<T> {
    pub child: Option<TreeIndex>,
    pub next: Option<TreeIndex>,
    pub item: T,
}

pub(crate) trait ListFlagStorage: Sequence<Node<Item>> {
    fn set_list_flags(&self, index: usize, flags: ListFlags) -> Result<(), Self::Error>;
}

impl ListFlagStorage for Vec<Node<Item>> {
    fn set_list_flags(
        &self,
        index: usize,
        flags: ListFlags,
    ) -> Result<(), std::convert::Infallible> {
        match &self[index].item.body {
            ItemBody::List(value, ..) | ItemBody::DefinitionList(value) => value.set(flags),
            _ => unreachable!("list flags require a list node"),
        }
        Ok(())
    }
}

#[cfg(feature = "disk")]
impl ListFlagStorage for crate::disk::Sequence<'_, Node<Item>> {
    fn set_list_flags(&self, index: usize, flags: ListFlags) -> std::io::Result<()> {
        let node = self.get(index as u64)?;
        match &node.item.body {
            ItemBody::List(value, ..) | ItemBody::DefinitionList(value) => value.set(flags),
            _ => unreachable!("list flags require a list node"),
        }
        self.set(index as u64, &node)
    }
}

#[derive(Clone)]
pub(crate) struct Tree<T, N = Vec<Node<T>>, P = Vec<TreeIndex>>
where
    N: Sequence<Node<T>>,
    P: Sequence<TreeIndex, Error = N::Error>,
{
    nodes: N,
    spine: P,
    cur: Option<TreeIndex>,
    marker: std::marker::PhantomData<T>,
}

impl<T: Default> Tree<T> {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        match Self::with_storage(Vec::with_capacity(capacity), Vec::new()) {
            Ok(tree) => tree,
            Err(error) => match error {},
        }
    }
}

impl<T: Default, N, P> Tree<T, N, P>
where
    N: Sequence<Node<T>>,
    P: Sequence<TreeIndex, Error = N::Error>,
{
    pub(crate) fn with_storage(mut nodes: N, spine: P) -> Result<Self, N::Error> {
        nodes.push(Node {
            child: None,
            next: None,
            item: T::default(),
        })?;
        Ok(Self {
            nodes,
            spine,
            cur: None,
            marker: std::marker::PhantomData,
        })
    }

    pub(crate) fn node(&self, index: TreeIndex) -> Result<N::Read<'_>, N::Error> {
        self.nodes.read(index.get())
    }

    pub(crate) fn update<R>(
        &mut self,
        index: TreeIndex,
        operation: impl FnOnce(&mut Node<T>) -> R,
    ) -> Result<R, N::Error> {
        self.nodes.update(index.get(), operation)
    }

    pub(crate) fn cur(&self) -> Option<TreeIndex> {
        self.cur
    }

    pub(crate) fn append(&mut self, item: T) -> Result<TreeIndex, N::Error> {
        let index = self.create_node(item)?;
        if let Some(current) = self.cur {
            self.update(current, |node| node.next = Some(index))?;
        } else if let Some(parent) = self.peek_up()? {
            self.update(parent, |node| node.child = Some(index))?;
        }
        self.cur = Some(index);
        Ok(index)
    }

    pub(crate) fn create_node(&mut self, item: T) -> Result<TreeIndex, N::Error> {
        let index = self.nodes.len();
        self.nodes.push(Node {
            child: None,
            next: None,
            item,
        })?;
        Ok(TreeIndex::new(index))
    }

    pub(crate) fn push(&mut self) -> Result<TreeIndex, N::Error> {
        let current = self.cur.unwrap();
        self.spine.push(current)?;
        let child = self.node(current)?.child;
        self.cur = child;
        Ok(current)
    }

    pub(crate) fn pop(&mut self) -> Result<Option<TreeIndex>, N::Error> {
        let index = self.spine.pop()?;
        self.cur = index;
        Ok(index)
    }

    pub(crate) fn remove_node(&mut self) -> Result<Option<TreeIndex>, N::Error> {
        let Some(index) = self.spine.pop()? else {
            return Ok(None);
        };
        self.cur = Some(index);
        self.nodes.pop()?;
        self.update(index, |node| node.child = None)?;
        Ok(Some(index))
    }

    pub(crate) fn peek_up(&self) -> Result<Option<TreeIndex>, N::Error> {
        self.spine
            .len()
            .checked_sub(1)
            .map(|index| self.spine.read(index).map(|value| *value))
            .transpose()
    }

    pub(crate) fn peek_grandparent(&self) -> Result<Option<TreeIndex>, N::Error> {
        self.spine
            .len()
            .checked_sub(2)
            .map(|index| self.spine.read(index).map(|value| *value))
            .transpose()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }

    pub(crate) fn spine_len(&self) -> usize {
        self.spine.len()
    }

    pub(crate) fn reset(&mut self) -> Result<(), N::Error> {
        self.cur = (!self.is_empty()).then(|| TreeIndex::new(1));
        self.spine.truncate(0)
    }

    pub(crate) fn walk_spine(&self) -> SpineIter<'_, P> {
        SpineIter {
            spine: &self.spine,
            front: 0,
            back: self.spine.len(),
        }
    }

    pub(crate) fn next_sibling(
        &mut self,
        current: TreeIndex,
    ) -> Result<Option<TreeIndex>, N::Error> {
        let next = self.node(current)?.next;
        self.cur = next;
        Ok(self.cur)
    }

    pub(crate) fn truncate_to_parent(&mut self, child: TreeIndex) -> Result<(), N::Error> {
        let next = self.node(child)?.next;
        self.update(child, |node| node.next = None)?;
        if let Some(current) = self.cur {
            self.update(current, |node| node.next = next)?;
        } else if let Some(parent) = self.peek_up()? {
            self.update(parent, |node| node.child = next)?;
        }
        if next.is_some() {
            self.cur = next;
        }
        Ok(())
    }
}

impl<N, P> Tree<Item, N, P>
where
    N: Sequence<Node<Item>>,
    P: Sequence<TreeIndex, Error = N::Error>,
{
    pub(crate) fn body_snapshot(&self, index: TreeIndex) -> Result<ItemBody, N::Error> {
        // Release the storage read before writing nodes.
        Ok(self.node(index)?.item.body.clone())
    }

    pub(crate) fn truncate_siblings(&mut self, end: usize) -> Result<(), N::Error> {
        let parent = self.peek_up()?.unwrap();
        let mut next_child = self.node(parent)?.child;
        let mut previous = None;
        while let Some(child) = next_child {
            let node = self.node(child)?;
            if node.item.end < end {
                previous = Some(child);
                next_child = node.next;
                continue;
            }
            let child_end = node.item.end;
            let child_start = node.item.start;
            let escaped = matches!(
                &node.item.body,
                ItemBody::Text {
                    backslash_escaped: true
                }
            );
            drop(node);
            if child_end == end {
                self.update(child, |node| node.next = None)?;
                self.cur = Some(child);
            } else if child_start == end {
                if escaped {
                    self.update(child, |node| {
                        node.item.start = end - 1;
                        node.item.end = end;
                    })?;
                    self.cur = Some(child);
                } else if let Some(previous) = previous {
                    self.update(previous, |node| node.next = None)?;
                    self.cur = Some(previous);
                } else {
                    self.update(parent, |node| node.child = None)?;
                    self.cur = None;
                }
            } else {
                debug_assert!(child_start < end && end < child_end);
                self.update(child, |node| {
                    node.item.end = end;
                    node.next = None;
                })?;
                self.cur = Some(child);
            }
            break;
        }
        Ok(())
    }
}

impl<N, P> Tree<Item, N, P>
where
    N: ListFlagStorage,
    P: Sequence<TreeIndex, Error = N::Error>,
{
    pub(crate) fn set_list_flags(
        &self,
        index: TreeIndex,
        flags: ListFlags,
    ) -> Result<(), N::Error> {
        self.nodes.set_list_flags(index.get(), flags)
    }
}

impl<T, N, P> std::fmt::Debug for Tree<T, N, P>
where
    N: Sequence<Node<T>>,
    P: Sequence<TreeIndex, Error = N::Error>,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Tree")
            .field("nodes", &self.nodes.len())
            .field("depth", &self.spine.len())
            .field("current", &self.cur)
            .finish()
    }
}

pub(crate) struct SpineIter<'a, P> {
    spine: &'a P,
    front: usize,
    back: usize,
}

impl<P: Sequence<TreeIndex>> Iterator for SpineIter<'_, P> {
    type Item = Result<TreeIndex, P::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        let index = self.front;
        self.front += 1;
        Some(self.spine.read(index).map(|value| *value))
    }

    fn nth(&mut self, count: usize) -> Option<Self::Item> {
        if count >= self.back - self.front {
            self.front = self.back;
            return None;
        }
        self.front += count;
        self.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.back - self.front;
        (remaining, Some(remaining))
    }
}

impl<P: Sequence<TreeIndex>> DoubleEndedIterator for SpineIter<'_, P> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        self.back -= 1;
        Some(self.spine.read(self.back).map(|value| *value))
    }

    fn nth_back(&mut self, count: usize) -> Option<Self::Item> {
        if count >= self.back - self.front {
            self.front = self.back;
            return None;
        }
        self.back -= count;
        self.next_back()
    }
}

impl<P: Sequence<TreeIndex>> ExactSizeIterator for SpineIter<'_, P> {}
