//! B+Tree: node views over pager pages.
//!
//! The node format is described in ADR 0005
//! (`docs/decisions/0005-btree-node-format.md`). This module has the node
//! level only: borrowed read and write views of one leaf or internal node, and
//! the cell encodings. Descending, splitting and the tree handle come in later
//! steps.
//!
//! - [`node`](self::node): size limits, [`node::ValueRef`] and the leaf cell codec.
//! - [`leaf`](self::leaf): [`leaf::LeafNode`] and [`leaf::LeafNodeMut`].
//! - [`internal`](self::internal): [`internal::InternalNode`] and [`internal::InternalNodeMut`].
//! - [`error`](self::error): [`error::NodeError`].

pub mod error;
pub mod internal;
pub mod leaf;
pub mod node;
