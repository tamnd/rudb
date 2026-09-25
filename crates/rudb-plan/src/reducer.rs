//! What a [`Node::Consistent`](crate::Node::Consistent) reads, and in which order.
//!
//! The node answers a MIN or MAX over an acyclic join without running the join. What makes that
//! possible is that the join's relations can be laid out as a tree in which every pair of joined
//! relations sharing a column class is connected by a path that carries it, the running
//! intersection property, and on such a tree two sweeps of semijoins leave exactly the rows that
//! take part in at least one joined row. The first sweep goes from the leaves up and the second
//! from the root down. Everything the executor needs to run them is here: the relations, which
//! columns of each are join keys and which class each key is in, which relation is each one's
//! parent and on which class, and which column of which relation each extreme is read from.
//!
//! The relations are stored in the order they are scanned, and that order is part of the contract
//! rather than a detail: every relation comes after all of its children. That is what lets the
//! first sweep happen while the relations are being scanned rather than after, since by the time a
//! relation's rows arrive the set of every child's keys is already built, and a row with no partner
//! in one of them is dropped before it is stored anywhere. A root is last in its tree.
//!
//! A plan can hold a forest rather than a tree, when the query joins groups of relations that
//! share no class and so are only a cross product of each other. Each tree is reduced on its own,
//! and the product is empty exactly when one of them is.

use rudb_common::{Error, Result};

use crate::NodeRef;

/// The relations of one consistent node and the join tree over them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reducer {
    /// One per relation, children before parents.
    pub leaves: Vec<Leaf>,
    /// How many classes of join columns there are, numbered from zero.
    pub classes: u32,
    /// One per produced column, in the order the node produces them.
    pub extremes: Vec<Extreme>,
}

/// One relation of the join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    /// The plan that produces its rows, which is a scan or a filter over a scan.
    ///
    /// The scan carries only the columns this node reads, which are the keys, the columns an
    /// extreme is read from and the columns the filter reads, and the positions below are
    /// positions in what this produces.
    pub input: NodeRef,
    /// Every join column of the relation, one per class it is in.
    pub keys: Vec<Key>,
    /// The relation this one hangs under in the join tree and the class they share, or nothing
    /// for a root.
    pub parent: Option<Edge>,
}

/// One join column of a relation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    /// Which class of equal columns it is in.
    pub class: u32,
    /// Its position in what the relation's input produces.
    pub column: u32,
}

/// The line from a relation to its parent in the join tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    /// The parent's position in [`Reducer::leaves`].
    pub leaf: u32,
    /// The one class the two share.
    pub class: u32,
}

/// One MIN or MAX the node produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extreme {
    /// The relation the column is in, as a position in [`Reducer::leaves`].
    pub leaf: u32,
    /// The column's position in what that relation's input produces.
    pub column: u32,
    /// Whether this is a MAX rather than a MIN.
    pub max: bool,
}

impl Reducer {
    /// The relations directly under `leaf`, each with the class it shares with `leaf`.
    pub fn children(&self, leaf: u32) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.leaves.iter().enumerate().filter_map(move |(child, held)| match held.parent {
            Some(edge) if edge.leaf == leaf => Some((position(child), edge.class)),
            _ => None,
        })
    }

    /// The column of `leaf` that is in `class`, if it has one.
    #[must_use]
    pub fn key(&self, leaf: u32, class: u32) -> Option<u32> {
        self.leaves[leaf as usize].keys.iter().find(|key| key.class == class).map(|key| key.column)
    }

    /// Whether some extreme is read from `leaf` or from a relation under it.
    ///
    /// The second sweep only has to reach the relations an extreme is read from, and a relation
    /// with no extreme anywhere under it is one the second sweep can leave alone: nothing that is
    /// read later depends on which of its rows survive it.
    #[must_use]
    pub fn wanted(&self, leaf: u32) -> bool {
        self.extremes.iter().any(|extreme| self.under(extreme.leaf, leaf))
    }

    /// Whether the rows of `leaf` have to be held until the second sweep reaches it.
    ///
    /// A root is reduced completely by the first sweep alone, since everything in its tree is
    /// under it, so its extremes and the keys it hands down are read off its rows as they are
    /// scanned. Every other relation the second sweep reaches has to keep its rows until its parent
    /// has been reduced, and that is the memory this node spends.
    #[must_use]
    pub fn held(&self, leaf: u32) -> bool {
        self.leaves[leaf as usize].parent.is_some() && self.wanted(leaf)
    }

    /// Whether `leaf` is `ancestor` or somewhere under it.
    fn under(&self, leaf: u32, ancestor: u32) -> bool {
        let mut at = Some(leaf);
        while let Some(here) = at {
            if here == ancestor {
                return true;
            }
            at = self.leaves[here as usize].parent.map(|edge| edge.leaf);
        }
        false
    }

    /// Checks the promises the executor relies on.
    ///
    /// A parent after its child, a class that both ends of an edge hold a column of, a class number
    /// that is in range and an extreme that names a relation there is. Each of these broken is a
    /// sweep that reads the wrong column or waits for a set that is built after it is needed, which
    /// is a wrong answer rather than an error, so it is checked where the plan is.
    ///
    /// # Errors
    ///
    /// Naming the relation that broke one.
    pub fn validate(&self) -> Result<()> {
        let count = self.leaves.len();
        for (at, leaf) in self.leaves.iter().enumerate() {
            let fail = |what: &str| Err(Error::internal(format!("relation {at} {what}")));
            if leaf.keys.iter().any(|key| key.class >= self.classes) {
                return fail("has a key in a class that is not there");
            }
            if let Some(edge) = leaf.parent {
                if edge.leaf as usize <= at || edge.leaf as usize >= count {
                    return fail("hangs under a relation that is not after it");
                }
                if self.key(position(at), edge.class).is_none()
                    || self.key(edge.leaf, edge.class).is_none()
                {
                    return fail(
                        "shares a class with its parent that one of the two has no column in",
                    );
                }
            }
        }
        if self.extremes.iter().any(|extreme| extreme.leaf as usize >= count) {
            return Err(Error::internal("an extreme is read from a relation that is not there"));
        }
        Ok(())
    }
}

/// A position in a list that came from a plan, which has fewer than `u32::MAX` of anything.
fn position(at: usize) -> u32 {
    u32::try_from(at).expect("a plan has fewer than u32::MAX relations")
}

#[cfg(test)]
mod tests {
    use super::{Edge, Extreme, Key, Leaf, Reducer};

    /// Three relations in a line, `a` under `b` under `c`, with the extreme read from `a`.
    fn line() -> Reducer {
        Reducer {
            leaves: vec![
                Leaf {
                    input: 0,
                    keys: vec![Key { class: 0, column: 0 }],
                    parent: Some(Edge { leaf: 1, class: 0 }),
                },
                Leaf {
                    input: 1,
                    keys: vec![Key { class: 0, column: 0 }, Key { class: 1, column: 1 }],
                    parent: Some(Edge { leaf: 2, class: 1 }),
                },
                Leaf { input: 2, keys: vec![Key { class: 1, column: 0 }], parent: None },
            ],
            classes: 2,
            extremes: vec![Extreme { leaf: 0, column: 1, max: false }],
        }
    }

    #[test]
    fn the_children_are_the_relations_that_name_it_as_their_parent() {
        let reducer = line();
        assert_eq!(reducer.children(2).collect::<Vec<_>>(), [(1, 1)]);
        assert_eq!(reducer.children(1).collect::<Vec<_>>(), [(0, 0)]);
        assert_eq!(reducer.children(0).count(), 0);
    }

    #[test]
    fn only_the_path_down_to_an_extreme_is_held() {
        let reducer = line();
        assert!(reducer.held(0), "the extreme is read here");
        assert!(reducer.held(1), "and the path to it passes through here");
        assert!(!reducer.held(2), "the root is reduced as it is scanned");
        let mut rooted = reducer;
        rooted.extremes = vec![Extreme { leaf: 2, column: 0, max: true }];
        assert!(!rooted.held(0) && !rooted.held(1), "nothing below the root is read again");
    }

    #[test]
    fn a_parent_before_its_child_is_refused() {
        let mut reducer = line();
        assert!(reducer.validate().is_ok());
        reducer.leaves[2].parent = Some(Edge { leaf: 0, class: 0 });
        assert!(reducer.validate().is_err());
    }

    #[test]
    fn an_edge_on_a_class_one_end_has_no_column_in_is_refused() {
        let mut reducer = line();
        reducer.leaves[0].parent = Some(Edge { leaf: 1, class: 1 });
        assert!(reducer.validate().is_err());
    }
}
