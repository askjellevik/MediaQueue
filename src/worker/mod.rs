//! The fetch worker's logic: reading release names, filtering and ranking
//! candidates. Nothing in here touches the network or the disk, so every
//! rule can be tested with plain strings.

pub mod release;
