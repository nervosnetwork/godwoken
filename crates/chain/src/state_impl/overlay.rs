//! Provide overlay store feature
//! Overlay store can be abandoned or commited.

use anyhow::Result;
use gw_common::{
    smt::SMT,
    sparse_merkle_tree::{
        error::Error as SMTError,
        traits::Store,
        tree::{BranchNode, LeafNode},
        H256,
    },
    state::{Error, State},
};
use std::collections::{HashMap, HashSet};

pub struct OverlayState<S> {
    tree: SMT<OverlayStore<S>>,
    account_count: u32,
}

impl<S: Store<H256>> OverlayState<S> {
    pub fn new(root: H256, store: S, account_count: u32) -> Self {
        let tree = SMT::new(root, OverlayStore::new(store));
        OverlayState {
            tree,
            account_count,
        }
    }

    pub fn overlay_store(&self) -> &OverlayStore<S> {
        self.tree.store()
    }

    pub fn overlay_store_mut(&mut self) -> &mut OverlayStore<S> {
        self.tree.store_mut()
    }
}

impl<S: Store<H256>> State for OverlayState<S> {
    fn get_raw(&self, key: &[u8; 32]) -> Result<[u8; 32], Error> {
        let v = self.tree.get(&(*key).into())?;
        Ok(v.into())
    }
    fn update_raw(&mut self, key: [u8; 32], value: [u8; 32]) -> Result<(), Error> {
        self.tree.update(key.into(), value.into())?;
        Ok(())
    }
    fn calculate_root(&self) -> Result<[u8; 32], Error> {
        let root = (*self.tree.root()).into();
        Ok(root)
    }
    fn get_account_count(&self) -> Result<u32, Error> {
        Ok(self.account_count)
    }
    fn set_account_count(&mut self, count: u32) -> Result<(), Error> {
        self.account_count = count;
        Ok(())
    }
}

pub struct OverlayStore<S> {
    store: S,
    branches_map: HashMap<H256, BranchNode>,
    leaves_map: HashMap<H256, LeafNode<H256>>,
    deleted_branches: HashSet<H256>,
    deleted_leaves: HashSet<H256>,
    touched_keys: HashSet<H256>,
}

impl<S: Store<H256>> OverlayStore<S> {
    pub fn new(store: S) -> Self {
        OverlayStore {
            store,
            branches_map: HashMap::default(),
            leaves_map: HashMap::default(),
            deleted_branches: HashSet::default(),
            deleted_leaves: HashSet::default(),
            touched_keys: HashSet::default(),
        }
    }

    pub fn touched_keys(&self) -> &HashSet<H256> {
        &self.touched_keys
    }

    pub fn clear_touched_keys(&mut self) {
        self.touched_keys.clear()
    }
}

impl<S: Store<H256>> Store<H256> for OverlayStore<S> {
    fn get_branch(&self, node: &H256) -> Result<Option<BranchNode>, SMTError> {
        if self.deleted_branches.contains(&node) {
            return Ok(None);
        }
        match self.branches_map.get(node) {
            Some(value) => Ok(Some(value.clone())),
            None => self.store.get_branch(node),
        }
    }
    fn get_leaf(&self, leaf_hash: &H256) -> Result<Option<LeafNode<H256>>, SMTError> {
        if self.deleted_leaves.contains(&leaf_hash) {
            return Ok(None);
        }
        match self.leaves_map.get(leaf_hash) {
            Some(value) => Ok(Some(value.clone())),
            None => self.store.get_leaf(leaf_hash),
        }
    }
    fn insert_branch(&mut self, node: H256, branch: BranchNode) -> Result<(), SMTError> {
        self.deleted_branches.remove(&node);
        self.branches_map.insert(node, branch);
        Ok(())
    }
    fn insert_leaf(&mut self, leaf_hash: H256, leaf: LeafNode<H256>) -> Result<(), SMTError> {
        self.deleted_leaves.remove(&leaf_hash);
        self.leaves_map.insert(leaf_hash, leaf);
        self.touched_keys.insert(leaf_hash);
        Ok(())
    }
    fn remove_branch(&mut self, node: &H256) -> Result<(), SMTError> {
        self.deleted_branches.insert(*node);
        self.branches_map.remove(node);
        Ok(())
    }
    fn remove_leaf(&mut self, leaf_hash: &H256) -> Result<(), SMTError> {
        self.deleted_leaves.insert(*leaf_hash);
        self.leaves_map.remove(leaf_hash);
        self.touched_keys.insert(*leaf_hash);
        Ok(())
    }
}