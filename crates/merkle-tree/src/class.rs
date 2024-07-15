use std::ops::ControlFlow;

use anyhow::Context;
use bitvec::order::Msb0;
use bitvec::prelude::BitSlice;
use pathfinder_common::hash::{PedersenHash, PoseidonHash};
use pathfinder_common::trie::TrieNode;
use pathfinder_common::{
    BlockNumber,
    CasmHash,
    ClassCommitment,
    ClassCommitmentLeafHash,
    ClassHash,
    SierraHash,
    StorageAddress,
    StorageValue,
};
use pathfinder_crypto::Felt;
use pathfinder_storage::{Transaction, TrieUpdate};

use crate::merkle_node::InternalNode;
use crate::tree::{MerkleTree, Visit};

/// A [Patricia Merkle tree](MerkleTree) used to calculate commitments to
/// Starknet's Sierra classes.
///
/// It maps a class's [SierraHash] to its [ClassCommitmentLeafHash]
///
/// Tree data is persisted by a sqlite table 'tree_class'.
pub struct ClassCommitmentTree<'tx> {
    tree: MerkleTree<PoseidonHash, 251>,
    storage: ClassStorage<'tx>,
}

impl<'tx> ClassCommitmentTree<'tx> {
    pub fn empty(tx: &'tx Transaction<'tx>) -> Self {
        let storage = ClassStorage { tx, block: None };
        let tree = MerkleTree::empty();

        Self { tree, storage }
    }

    pub fn load(tx: &'tx Transaction<'tx>, block: BlockNumber) -> anyhow::Result<Self> {
        let root = tx
            .class_root_index(block)
            .context("Querying class root index")?;
        let Some(root) = root else {
            return Ok(Self::empty(tx));
        };

        let storage = ClassStorage {
            tx,
            block: Some(block),
        };
        let tree = MerkleTree::new(root);

        Ok(Self { tree, storage })
    }

    pub fn with_verify_hashes(mut self, verify_hashes: bool) -> Self {
        self.tree = self.tree.with_verify_hashes(verify_hashes);
        self
    }

    /// Adds a leaf node for a Sierra -> CASM commitment.
    ///
    /// Note that the leaf value is _not_ the Cairo hash, but a hashed value
    /// based on that. See <https://github.com/starkware-libs/cairo-lang/blob/12ca9e91bbdc8a423c63280949c7e34382792067/src/starkware/starknet/core/os/state.cairo#L302>
    /// for details.
    pub fn set(&mut self, class: SierraHash, value: ClassCommitmentLeafHash) -> anyhow::Result<()> {
        let key = class.view_bits().to_owned();
        self.tree.set(&self.storage, key, value.0)
    }

    /// Commits the changes and calculates the new node hashes. Returns the new
    /// commitment and any potentially newly created nodes.
    pub fn commit(self) -> anyhow::Result<(ClassCommitment, TrieUpdate)> {
        let update = self.tree.commit(&self.storage)?;

        let commitment = ClassCommitment(update.root_commitment);
        Ok((commitment, update))
    }

    /// Generates a proof for a given `key`
    pub fn get_proof(
        tx: &'tx Transaction<'tx>,
        block: BlockNumber,
        class_hash: ClassHash,
    ) -> anyhow::Result<Option<Vec<TrieNode>>> {
        let root = tx
            .class_root_index(block)
            .context("Querying class root index")?;

        let Some(root) = root else {
            return Ok(None);
        };

        let storage = ClassTrieStorage {
            tx,
            block: Some(block),
        };

        let casm = tx
            .casm_hash_at(block.into(), class_hash)
            .context("Querying CASM hash")?;

        let Some(casm) = casm else {
            return Ok(None);
        };

        MerkleTree::<PoseidonHash, 251>::get_proof(root, &storage, casm.view_bits())
    }
}

/// A [Patricia Merkle tree](MerkleTree) used to calculate commitments to
/// Starknet's Sierra classes.
///
/// It maps a class's [SierraHash] to its [ClassCommitmentLeafHash]
///
/// Tree data is persisted by a sqlite table 'tree_class'.

pub struct ClassStorageTree<'tx> {
    tree: MerkleTree<PoseidonHash, 251>,
    storage: ClassStorage<'tx>,
}

impl<'tx> ClassStorageTree<'tx> {
    pub fn empty(tx: &'tx Transaction<'tx>) -> Self {
        let storage = ClassStorage { tx, block: None };
        let tree = MerkleTree::empty();

        Self { tree, storage }
    }

    pub fn load(tx: &'tx Transaction<'tx>, block: BlockNumber) -> anyhow::Result<Self> {
        let root = tx
            .class_root_index(block)
            .context("Querying class root index")?;

        let Some(root) = root else {
            return Ok(Self::empty(tx));
        };

        let storage = ClassStorage {
            tx,
            block: Some(block),
        };

        let tree = MerkleTree::new(root);

        Ok(Self { tree, storage })
    }

    pub fn with_verify_hashes(mut self, verify_hashes: bool) -> Self {
        self.tree = self.tree.with_verify_hashes(verify_hashes);
        self
    }

    /// Generates a proof for `key`. See [`MerkleTree::get_proof`].
    pub fn get_proof(
        tx: &'tx Transaction<'tx>,
        block: BlockNumber,
        key: &BitSlice<u8, Msb0>,
    ) -> anyhow::Result<Option<Vec<TrieNode>>> {
        let root = tx
            .class_root_index(block)
            .context("Querying class root index")?;

        let Some(root) = root else {
            return Ok(None);
        };

        let storage = ClassStorage {
            tx,
            block: Some(block),
        };

        MerkleTree::<PedersenHash, 251>::get_proof(root, &storage, key)
    }

    pub fn set(&mut self, address: StorageAddress, value: StorageValue) -> anyhow::Result<()> {
        let key = address.view_bits().to_owned();
        self.tree.set(&self.storage, key, value.0)
    }

    /// Commits the changes and calculates the new node hashes. Returns the new
    /// commitment and any potentially newly created nodes.
    pub fn commit(self) -> anyhow::Result<(CasmHash, TrieUpdate)> {
        let update = self.tree.commit(&self.storage)?;
        let commitment = CasmHash(update.root_commitment);
        Ok((commitment, update))
    }

    /// See [`MerkleTree::dfs`]
    pub fn dfs<B, F: FnMut(&InternalNode, &BitSlice<u8, Msb0>) -> ControlFlow<B, Visit>>(
        &mut self,
        f: &mut F,
    ) -> anyhow::Result<Option<B>> {
        self.tree.dfs(&self.storage, f)
    }
}

struct ClassTrieStorage<'tx> {
    tx: &'tx Transaction<'tx>,
    block: Option<BlockNumber>,
}

impl crate::storage::Storage for ClassTrieStorage<'_> {
    fn get(&self, index: u64) -> anyhow::Result<Option<pathfinder_storage::StoredNode>> {
        self.tx.storage_trie_node(index)
    }

    fn hash(&self, index: u64) -> anyhow::Result<Option<Felt>> {
        self.tx.storage_trie_node_hash(index)
    }

    fn leaf(&self, path: &BitSlice<u8, Msb0>) -> anyhow::Result<Option<Felt>> {
        assert!(path.len() == 251);

        let Some(block) = self.block else {
            return Ok(None);
        };

        let sierra =
            ClassHash(Felt::from_bits(path).context("Mapping leaf path to contract address")?);

        let casm = self
            .tx
            .casm_hash_at(block.into(), sierra)
            .context("Querying CASM hash")?;
        let Some(casm) = casm else {
            return Ok(None);
        };

        let value = self.tx.class_commitment_leaf(block, &casm)?.map(|x| x.0);

        Ok(value)
    }
}

struct ClassStorage<'tx> {
    tx: &'tx Transaction<'tx>,
    block: Option<BlockNumber>,
}

impl crate::storage::Storage for ClassStorage<'_> {
    fn get(&self, index: u64) -> anyhow::Result<Option<pathfinder_storage::StoredNode>> {
        self.tx.class_trie_node(index)
    }

    fn hash(&self, index: u64) -> anyhow::Result<Option<Felt>> {
        self.tx.class_trie_node_hash(index)
    }

    fn leaf(
        &self,
        path: &bitvec::slice::BitSlice<u8, bitvec::prelude::Msb0>,
    ) -> anyhow::Result<Option<Felt>> {
        assert!(path.len() == 251);

        let Some(block) = self.block else {
            return Ok(None);
        };

        let sierra = ClassHash(Felt::from_bits(path).context("Mapping path to sierra hash")?);

        let casm = self
            .tx
            .casm_hash_at(block.into(), sierra)
            .context("Querying CASM hash")?;
        let Some(casm) = casm else {
            return Ok(None);
        };

        let leaf = self
            .tx
            .class_commitment_leaf(block, &casm)
            .context("Querying class leaf")?
            .map(|x| x.0);

        Ok(leaf)
    }
}
