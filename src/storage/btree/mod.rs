//! B+Tree: node views over pager pages.
//!
//! The node format is described in ADR 0005
//! (`docs/decisions/0005-btree-node-format.md`). This module has the node
//! level (borrowed read and write views of one leaf or internal node, and the
//! cell encodings) and the tree handle with lookup, insert and leaf splits.
//! Overflow chains for large values live in [`overflow`](self::overflow).
//! Scans and delete come in later steps.
//!
//! - [`node`](self::node): size limits, [`node::ValueRef`] and the leaf cell codec.
//! - [`leaf`](self::leaf): [`leaf::LeafNode`] and [`leaf::LeafNodeMut`].
//! - [`internal`](self::internal): [`internal::InternalNode`] and [`internal::InternalNodeMut`].
//! - [`overflow`](self::overflow): overflow page views and chain read, write and free.
//! - [`tree`](self::tree): [`tree::BTree`], the handle of one tree.
//! - [`error`](self::error): [`error::NodeError`] and [`error::BTreeError`].

pub mod error;
pub mod internal;
pub mod leaf;
pub mod node;
pub mod overflow;
pub mod tree;

pub use tree::BTree;
