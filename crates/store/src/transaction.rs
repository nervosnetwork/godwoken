use crate::{smt_store_impl::SMTStore, traits::KVStore};
use gw_common::h256_ext::H256Ext;
use gw_common::{merkle_utils::calculate_state_checkpoint, smt::SMT, CKB_SUDT_SCRIPT_ARGS, H256};
use gw_db::schema::{
    Col, COLUMN_BAD_BLOCK_CHALLENGE_TARGET, COLUMN_BLOCK, COLUMN_BLOCK_DEPOSIT_REQUESTS,
    COLUMN_BLOCK_GLOBAL_STATE, COLUMN_BLOCK_SMT_BRANCH, COLUMN_BLOCK_SMT_LEAF,
    COLUMN_BLOCK_STATE_RECORD, COLUMN_CHECKPOINT, COLUMN_CUSTODIAN_ASSETS, COLUMN_INDEX,
    COLUMN_L2BLOCK_COMMITTED_INFO, COLUMN_META, COLUMN_REVERTED_BLOCK_SMT_BRANCH,
    COLUMN_REVERTED_BLOCK_SMT_LEAF, COLUMN_REVERTED_BLOCK_SMT_ROOT, COLUMN_TRANSACTION,
    COLUMN_TRANSACTION_INFO, COLUMN_TRANSACTION_RECEIPT, META_ACCOUNT_SMT_COUNT_KEY,
    META_ACCOUNT_SMT_ROOT_KEY, META_BLOCK_SMT_ROOT_KEY, META_CHAIN_ID_KEY,
    META_LAST_VALID_TIP_BLOCK_HASH_KEY, META_REVERTED_BLOCK_SMT_ROOT_KEY, META_TIP_BLOCK_HASH_KEY,
};
use gw_db::{
    error::Error, iter::DBIter, DBIterator, Direction::Forward, IteratorMode, RocksDBTransaction,
};
use gw_types::{
    packed::{
        self, AccountMerkleState, Byte32, ChallengeTarget, RollupConfig, TransactionKey,
        WithdrawalReceipt,
    },
    prelude::*,
};
use std::collections::HashSet;
use std::{borrow::BorrowMut, collections::HashMap};

const NUMBER_OF_CONFIRMATION: u64 = 100;

pub struct StoreTransaction {
    pub(crate) inner: RocksDBTransaction,
}

impl KVStore for StoreTransaction {
    fn get(&self, col: Col, key: &[u8]) -> Option<Box<[u8]>> {
        self.inner
            .get(col, key)
            .expect("db operation should be ok")
            .map(|v| Box::<[u8]>::from(v.as_ref()))
    }

    fn get_iter(&self, col: Col, mode: IteratorMode) -> DBIter {
        self.inner
            .iter(col, mode)
            .expect("db operation should be ok")
    }

    fn insert_raw(&self, col: Col, key: &[u8], value: &[u8]) -> Result<(), Error> {
        self.inner.put(col, key, value)
    }

    fn delete(&self, col: Col, key: &[u8]) -> Result<(), Error> {
        self.inner.delete(col, key)
    }
}

impl StoreTransaction {
    pub fn commit(&self) -> Result<(), Error> {
        self.inner.commit()
    }

    pub fn rollback(&self) -> Result<(), Error> {
        self.inner.rollback()
    }

    pub fn setup_chain_id(&self, chain_id: H256) -> Result<(), Error> {
        self.insert_raw(COLUMN_META, META_CHAIN_ID_KEY, chain_id.as_slice())?;
        Ok(())
    }

    pub fn get_block_smt_root(&self) -> Result<H256, Error> {
        let slice = self
            .get(COLUMN_META, META_BLOCK_SMT_ROOT_KEY)
            .expect("must has root");
        debug_assert_eq!(slice.len(), 32);
        let mut root = [0u8; 32];
        root.copy_from_slice(&slice);
        Ok(root.into())
    }

    pub fn set_block_smt_root(&self, root: H256) -> Result<(), Error> {
        self.insert_raw(COLUMN_META, META_BLOCK_SMT_ROOT_KEY, root.as_slice())?;
        Ok(())
    }

    pub fn block_smt(&self) -> Result<SMT<SMTStore<'_, Self>>, Error> {
        let root = self.get_block_smt_root()?;
        let smt_store = SMTStore::new(COLUMN_BLOCK_SMT_LEAF, COLUMN_BLOCK_SMT_BRANCH, self);
        Ok(SMT::new(root, smt_store))
    }

    pub fn get_reverted_block_smt_root(&self) -> Result<H256, Error> {
        let slice = self
            .get(COLUMN_META, META_REVERTED_BLOCK_SMT_ROOT_KEY)
            .expect("must has root");
        debug_assert_eq!(slice.len(), 32);
        let mut root = [0u8; 32];
        root.copy_from_slice(&slice);
        Ok(root.into())
    }

    pub fn set_reverted_block_smt_root(&self, root: H256) -> Result<(), Error> {
        self.insert_raw(
            COLUMN_META,
            META_REVERTED_BLOCK_SMT_ROOT_KEY,
            root.as_slice(),
        )?;
        Ok(())
    }

    pub fn reverted_block_smt(&self) -> Result<SMT<SMTStore<'_, Self>>, Error> {
        let root = self.get_reverted_block_smt_root()?;
        let smt_store = SMTStore::new(
            COLUMN_REVERTED_BLOCK_SMT_LEAF,
            COLUMN_REVERTED_BLOCK_SMT_BRANCH,
            self,
        );
        Ok(SMT::new(root, smt_store))
    }

    pub fn get_account_smt_root(&self) -> Result<H256, Error> {
        let slice = self
            .get(COLUMN_META, META_ACCOUNT_SMT_ROOT_KEY)
            .expect("must has root");
        debug_assert_eq!(slice.len(), 32);
        let mut root = [0u8; 32];
        root.copy_from_slice(&slice);
        Ok(root.into())
    }

    pub fn set_account_smt_root(&self, root: H256) -> Result<(), Error> {
        self.insert_raw(COLUMN_META, META_ACCOUNT_SMT_ROOT_KEY, root.as_slice())?;
        Ok(())
    }

    pub fn set_account_count(&self, count: u32) -> Result<(), Error> {
        let count: packed::Uint32 = count.pack();
        self.insert_raw(COLUMN_META, META_ACCOUNT_SMT_COUNT_KEY, count.as_slice())
            .expect("insert");
        Ok(())
    }

    pub fn get_account_count(&self) -> Result<u32, Error> {
        let slice = self
            .get(COLUMN_META, META_ACCOUNT_SMT_COUNT_KEY)
            .expect("account count");
        let count = packed::Uint32Reader::from_slice_should_be_ok(&slice.as_ref()).to_entity();
        Ok(count.unpack())
    }

    pub fn get_last_valid_tip_block(&self) -> Result<packed::L2Block, Error> {
        let block_hash = self.get_last_valid_tip_block_hash()?;
        let block = self
            .get_block(&block_hash)?
            .expect("last valid tip block exists");

        Ok(block)
    }

    pub fn get_last_valid_tip_block_hash(&self) -> Result<H256, Error> {
        let slice = self
            .get(COLUMN_META, META_LAST_VALID_TIP_BLOCK_HASH_KEY)
            .expect("get last valid tip block hash");

        let byte32 = packed::Byte32Reader::from_slice_should_be_ok(&slice.as_ref()).to_entity();
        Ok(byte32.unpack())
    }

    pub fn set_last_valid_tip_block_hash(&self, block_hash: &H256) -> Result<(), Error> {
        self.insert_raw(
            COLUMN_META,
            &META_LAST_VALID_TIP_BLOCK_HASH_KEY,
            block_hash.as_slice(),
        )
    }

    pub fn get_tip_block_hash(&self) -> Result<H256, Error> {
        let slice = self
            .get(COLUMN_META, META_TIP_BLOCK_HASH_KEY)
            .expect("get tip block hash");
        Ok(
            packed::Byte32Reader::from_slice_should_be_ok(&slice.as_ref())
                .to_entity()
                .unpack(),
        )
    }

    pub fn set_tip_block_hash(&self, block_hash: H256) -> Result<(), Error> {
        let block_hash: [u8; 32] = block_hash.into();
        self.insert_raw(COLUMN_META, &META_TIP_BLOCK_HASH_KEY, &block_hash)
    }

    pub fn get_tip_block(&self) -> Result<packed::L2Block, Error> {
        let tip_block_hash = self.get_tip_block_hash()?;
        Ok(self.get_block(&tip_block_hash)?.expect("get tip block"))
    }

    pub fn get_block_hash_by_number(&self, number: u64) -> Result<Option<H256>, Error> {
        let block_number: packed::Uint64 = number.pack();
        match self.get(COLUMN_INDEX, block_number.as_slice()) {
            Some(slice) => Ok(Some(
                packed::Byte32Reader::from_slice_should_be_ok(&slice.as_ref())
                    .to_entity()
                    .unpack(),
            )),
            None => Ok(None),
        }
    }

    pub fn get_block_number(&self, block_hash: &H256) -> Result<Option<u64>, Error> {
        match self.get(COLUMN_INDEX, block_hash.as_slice()) {
            Some(slice) => Ok(Some(
                packed::Uint64Reader::from_slice_should_be_ok(&slice.as_ref())
                    .to_entity()
                    .unpack(),
            )),
            None => Ok(None),
        }
    }

    pub fn get_block(&self, block_hash: &H256) -> Result<Option<packed::L2Block>, Error> {
        match self.get(COLUMN_BLOCK, block_hash.as_slice()) {
            Some(slice) => Ok(Some(
                packed::L2BlockReader::from_slice_should_be_ok(&slice.as_ref()).to_entity(),
            )),
            None => Ok(None),
        }
    }

    pub fn get_transaction(&self, tx_hash: &H256) -> Result<Option<packed::L2Transaction>, Error> {
        if let Some(slice) = self.get(COLUMN_TRANSACTION_INFO, tx_hash.as_slice()) {
            let info =
                packed::TransactionInfoReader::from_slice_should_be_ok(&slice.as_ref()).to_entity();
            let tx_key = info.key();
            Ok(self
                .get(COLUMN_TRANSACTION, &tx_key.as_slice())
                .map(|slice| {
                    packed::L2TransactionReader::from_slice_should_be_ok(&slice.as_ref())
                        .to_entity()
                }))
        } else {
            Ok(None)
        }
    }

    pub fn get_transaction_receipt(
        &self,
        tx_hash: &H256,
    ) -> Result<Option<packed::TxReceipt>, Error> {
        if let Some(slice) = self.get(COLUMN_TRANSACTION_INFO, tx_hash.as_slice()) {
            let info =
                packed::TransactionInfoReader::from_slice_should_be_ok(&slice.as_ref()).to_entity();
            let tx_key = info.key();
            self.get_transaction_receipt_by_key(&tx_key)
        } else {
            Ok(None)
        }
    }

    pub fn get_transaction_receipt_by_key(
        &self,
        key: &TransactionKey,
    ) -> Result<Option<packed::TxReceipt>, Error> {
        Ok(self
            .get(COLUMN_TRANSACTION_RECEIPT, &key.as_slice())
            .map(|slice| {
                packed::TxReceiptReader::from_slice_should_be_ok(&slice.as_ref()).to_entity()
            }))
    }

    pub fn get_checkpoint_post_state(
        &self,
        checkpoint: &Byte32,
    ) -> Result<Option<packed::AccountMerkleState>, Error> {
        Ok(self
            .get(COLUMN_CHECKPOINT, checkpoint.as_slice())
            .map(|slice| {
                packed::AccountMerkleStateReader::from_slice_should_be_ok(&slice.as_ref())
                    .to_entity()
            }))
    }

    pub fn get_l2block_committed_info(
        &self,
        block_hash: &H256,
    ) -> Result<Option<packed::L2BlockCommittedInfo>, Error> {
        match self.get(COLUMN_L2BLOCK_COMMITTED_INFO, block_hash.as_slice()) {
            Some(slice) => Ok(Some(
                packed::L2BlockCommittedInfoReader::from_slice_should_be_ok(&slice.as_ref())
                    .to_entity(),
            )),
            None => Ok(None),
        }
    }

    pub fn get_block_deposit_requests(
        &self,
        block_hash: &H256,
    ) -> Result<Option<Vec<packed::DepositRequest>>, Error> {
        match self.get(COLUMN_BLOCK_DEPOSIT_REQUESTS, block_hash.as_slice()) {
            Some(slice) => Ok(Some(
                packed::DepositRequestVecReader::from_slice_should_be_ok(&slice.as_ref())
                    .to_entity()
                    .into_iter()
                    .collect(),
            )),
            None => Ok(None),
        }
    }

    pub fn get_block_post_global_state(
        &self,
        block_hash: &H256,
    ) -> Result<Option<packed::GlobalState>, Error> {
        match self.get(COLUMN_BLOCK_GLOBAL_STATE, block_hash.as_slice()) {
            Some(slice) => Ok(Some(
                packed::GlobalStateReader::from_slice_should_be_ok(&slice.as_ref()).to_entity(),
            )),
            None => Ok(None),
        }
    }

    /// key: sudt_script_hash
    fn set_custodian_asset(&self, key: H256, value: u128) -> Result<(), Error> {
        self.insert_raw(
            COLUMN_CUSTODIAN_ASSETS,
            key.as_slice(),
            &value.to_le_bytes(),
        )
    }

    /// key: sudt_script_hash
    pub fn get_finalized_custodian_asset(&self, key: H256) -> Result<u128, Error> {
        match self.get(COLUMN_CUSTODIAN_ASSETS, key.as_slice()) {
            Some(slice) => {
                let mut buf = [0u8; 16];
                buf.copy_from_slice(&slice);
                Ok(u128::from_le_bytes(buf))
            }
            None => Ok(0),
        }
    }

    pub fn get_bad_block_challenge_target(
        &self,
        block_hash: &H256,
    ) -> Result<Option<ChallengeTarget>, Error> {
        match self.get(COLUMN_BAD_BLOCK_CHALLENGE_TARGET, &block_hash.as_slice()) {
            Some(slice) => {
                let target =
                    packed::ChallengeTargetReader::from_slice_should_be_ok(&slice.as_ref());
                Ok(Some(target.to_entity()))
            }
            None => Ok(None),
        }
    }

    pub fn set_bad_block_challenge_target(
        &self,
        block_hash: &H256,
        target: &ChallengeTarget,
    ) -> Result<(), Error> {
        self.insert_raw(
            COLUMN_BAD_BLOCK_CHALLENGE_TARGET,
            block_hash.as_slice(),
            target.as_slice(),
        )
    }

    // TODO: prune db state
    pub fn get_reverted_block_hashes(&self) -> Result<HashSet<H256>, Error> {
        let iter = self.get_iter(COLUMN_REVERTED_BLOCK_SMT_LEAF, IteratorMode::End);
        let to_byte32 = iter.map(|(key, _value)| {
            packed::Byte32Reader::from_slice_should_be_ok(key.as_ref()).to_entity()
        });
        let to_h256 = to_byte32.map(|byte32| byte32.unpack());

        Ok(to_h256.collect())
    }

    pub fn get_reverted_block_hashes_by_root(
        &self,
        reverted_block_smt_root: &H256,
    ) -> Result<Option<Vec<H256>>, Error> {
        match self.get(
            COLUMN_REVERTED_BLOCK_SMT_ROOT,
            reverted_block_smt_root.as_slice(),
        ) {
            Some(slice) => {
                let block_hash = packed::Byte32VecReader::from_slice_should_be_ok(&slice.as_ref());
                Ok(Some(block_hash.to_entity().unpack()))
            }
            None => Ok(None),
        }
    }

    pub fn set_reverted_block_hashes(
        &self,
        reverted_block_smt_root: &H256,
        block_hashes: Vec<H256>,
    ) -> Result<(), Error> {
        assert!(!block_hashes.is_empty(), "set empty reverted block hashes");

        self.insert_raw(
            COLUMN_REVERTED_BLOCK_SMT_ROOT,
            reverted_block_smt_root.as_slice(),
            block_hashes.pack().as_slice(),
        )
    }

    pub fn rewind_reverted_block_smt(&self, block_hashes: Vec<H256>) -> Result<(), Error> {
        let mut reverted_block_smt = self.reverted_block_smt()?;

        for block_hash in block_hashes.into_iter() {
            reverted_block_smt
                .update(block_hash, H256::zero())
                .map_err(|err| Error::from(format!("reset reverted block smt error {}", err)))?;
        }

        self.set_reverted_block_smt_root(*reverted_block_smt.root())
    }

    pub fn rewind_block_smt(&self, block: &packed::L2Block) -> Result<(), Error> {
        let mut block_smt = self.block_smt()?;
        block_smt
            .update(block.smt_key().into(), H256::zero())
            .map_err(|err| Error::from(format!("reset block smt error {}", err)))?;

        self.set_block_smt_root(*block_smt.root())
    }

    pub fn insert_bad_block(
        &self,
        block: &packed::L2Block,
        committed_info: &packed::L2BlockCommittedInfo,
        global_state: &packed::GlobalState,
    ) -> Result<(), Error> {
        let block_hash = block.hash();
        let block_number = block.raw().number();

        let committed_info = committed_info.as_slice();
        let global_state = global_state.as_slice();

        self.insert_raw(COLUMN_L2BLOCK_COMMITTED_INFO, &block_hash, committed_info)?;
        self.insert_raw(COLUMN_BLOCK_GLOBAL_STATE, &block_hash, global_state)?;

        self.insert_raw(COLUMN_BLOCK, &block_hash, block.as_slice())?;

        self.insert_raw(COLUMN_INDEX, block_number.as_slice(), &block_hash)?;
        self.insert_raw(COLUMN_INDEX, &block_hash, block_number.as_slice())?;

        // Add to block smt
        let mut block_smt = self.block_smt()?;
        block_smt
            .update(block.smt_key().into(), block_hash.into())
            .map_err(|err| Error::from(format!("block smt error {}", err)))?;
        self.set_block_smt_root(*block_smt.root())?;

        // Update tip block
        self.insert_raw(COLUMN_META, &META_TIP_BLOCK_HASH_KEY, &block_hash)?;

        Ok(())
    }

    pub fn revert_bad_blocks(&self, bad_blocks: &[packed::L2Block]) -> Result<(), Error> {
        if bad_blocks.is_empty() {
            return Ok(());
        }

        let mut block_smt = self.block_smt()?;
        let mut reverted_block_smt = self.reverted_block_smt()?;

        for block in bad_blocks {
            let block_hash = block.hash();
            let block_number = block.raw().number();

            self.delete(COLUMN_INDEX, &block_hash)?;
            self.delete(COLUMN_INDEX, block_number.as_slice())?;

            // Remove block from smt
            block_smt
                .update(block.smt_key().into(), H256::zero())
                .map_err(|err| Error::from(format!("block smt error {}", err)))?;

            // Add block to reverted smt
            reverted_block_smt
                .update(block_hash.into(), H256::one())
                .map_err(|err| Error::from(format!("reverted block smt error {}", err)))?;
        }

        self.set_block_smt_root(*block_smt.root())?;
        self.set_reverted_block_smt_root(*reverted_block_smt.root())?;

        // Revert tip block to parent block
        let parent_block_hash: [u8; 32] = {
            let first_bad_block = bad_blocks.first().expect("exists");
            first_bad_block.raw().parent_block_hash().unpack()
        };
        self.insert_raw(COLUMN_META, &META_TIP_BLOCK_HASH_KEY, &parent_block_hash)
    }

    #[allow(clippy::clippy::too_many_arguments)]
    pub fn insert_block(
        &self,
        block: packed::L2Block,
        committed_info: packed::L2BlockCommittedInfo,
        global_state: packed::GlobalState,
        withdrawal_receipts: Vec<WithdrawalReceipt>,
        prev_txs_state: AccountMerkleState,
        tx_receipts: Vec<packed::TxReceipt>,
        deposit_requests: Vec<packed::DepositRequest>,
    ) -> Result<(), Error> {
        debug_assert_eq!(block.transactions().len(), tx_receipts.len());
        let block_hash = block.hash();
        self.insert_raw(COLUMN_BLOCK, &block_hash, block.as_slice())?;
        self.insert_raw(
            COLUMN_L2BLOCK_COMMITTED_INFO,
            &block_hash,
            committed_info.as_slice(),
        )?;
        self.insert_raw(
            COLUMN_BLOCK_GLOBAL_STATE,
            &block_hash,
            global_state.as_slice(),
        )?;
        let deposit_requests_vec: packed::DepositRequestVec = deposit_requests.pack();
        self.insert_raw(
            COLUMN_BLOCK_DEPOSIT_REQUESTS,
            &block_hash,
            deposit_requests_vec.as_slice(),
        )?;

        // Verify prev tx state and insert
        {
            let prev_txs_state_checkpoint = {
                let txs = block.as_reader().raw().submit_transactions();
                txs.prev_state_checkpoint().to_entity()
            };

            let root: [u8; 32] = prev_txs_state.merkle_root().unpack();
            let count: u32 = prev_txs_state.count().unpack();
            let checkpoint: Byte32 = {
                let checkpoint: [u8; 32] = calculate_state_checkpoint(&root.into(), count).into();
                checkpoint.pack()
            };
            if checkpoint != prev_txs_state_checkpoint {
                return Err(Error::from("unexpected prev tx state".to_string()));
            }

            let block_post_state = block.as_reader().raw().post_account();
            if tx_receipts.is_empty() && prev_txs_state.as_slice() != block_post_state.as_slice() {
                return Err(Error::from("unexpected no tx post state".to_string()));
            }

            self.insert_raw(
                COLUMN_CHECKPOINT,
                checkpoint.as_slice(),
                prev_txs_state.as_slice(),
            )?;
        }

        for (index, (tx, tx_receipt)) in block
            .transactions()
            .into_iter()
            .zip(tx_receipts.iter())
            .enumerate()
        {
            let key = TransactionKey::build_transaction_key(block_hash.pack(), index as u32);
            self.insert_raw(COLUMN_TRANSACTION, &key.as_slice(), tx.as_slice())?;
            self.insert_raw(
                COLUMN_TRANSACTION_RECEIPT,
                &key.as_slice(),
                tx_receipt.as_slice(),
            )?;
        }

        let post_states: Vec<AccountMerkleState> = {
            let withdrawal_post_states = withdrawal_receipts.into_iter().map(|w| w.post_state());
            let tx_post_states = tx_receipts.iter().map(|t| t.post_state());
            withdrawal_post_states.chain(tx_post_states).collect()
        };

        let state_checkpoint_list = block.raw().state_checkpoint_list().into_iter();
        if post_states.len() != state_checkpoint_list.len() {
            return Err(Error::from("unexpected block post state length".to_owned()));
        }
        for (index, (checkpoint, post_state)) in state_checkpoint_list.zip(post_states).enumerate()
        {
            let root: [u8; 32] = post_state.merkle_root().unpack();
            let state_checkpoint: Byte32 = {
                let checkpoint: [u8; 32] =
                    calculate_state_checkpoint(&root.into(), post_state.count().unpack()).into();
                checkpoint.pack()
            };
            if state_checkpoint != checkpoint {
                return Err(Error::from(format!("unexpected post state {}", index)));
            }

            self.insert_raw(
                COLUMN_CHECKPOINT,
                checkpoint.as_slice(),
                post_state.as_slice(),
            )?;
        }

        Ok(())
    }

    /// Update finalized custodian assets
    fn update_finalized_custodian_assets<
        AddIter: Iterator<Item = CustodianChange>,
        RemIter: Iterator<Item = CustodianChange>,
    >(
        &self,
        addition: AddIter,
        removed: RemIter,
    ) -> Result<(), Error> {
        let mut touched_custodian_assets: HashMap<H256, u128> = Default::default();
        for request in addition {
            let CustodianChange {
                sudt_script_hash,
                amount,
                capacity,
            } = request;

            // update ckb balance
            let ckb_balance = touched_custodian_assets
                .entry(CKB_SUDT_SCRIPT_ARGS.into())
                .or_insert_with(|| {
                    self.get_finalized_custodian_asset(CKB_SUDT_SCRIPT_ARGS.into())
                        .expect("get custodian asset")
                })
                .borrow_mut();
            *ckb_balance = ckb_balance
                .checked_add(capacity as u128)
                .expect("deposit overflow");

            // update sUDT balance
            let balance = touched_custodian_assets
                .entry(sudt_script_hash)
                .or_insert_with(|| {
                    self.get_finalized_custodian_asset(sudt_script_hash)
                        .expect("get custodian asset")
                })
                .borrow_mut();
            *balance = balance.checked_add(amount).expect("deposit overflow");
        }
        for request in removed {
            let CustodianChange {
                sudt_script_hash,
                amount,
                capacity,
            } = request;

            // update ckb balance
            let ckb_balance = touched_custodian_assets
                .entry(CKB_SUDT_SCRIPT_ARGS.into())
                .or_insert_with(|| {
                    self.get_finalized_custodian_asset(CKB_SUDT_SCRIPT_ARGS.into())
                        .expect("get custodian asset")
                })
                .borrow_mut();

            *ckb_balance = ckb_balance
                .checked_sub(capacity as u128)
                .expect("withdrawal overflow");

            // update sUDT balance
            let balance = touched_custodian_assets
                .entry(sudt_script_hash)
                .or_insert_with(|| {
                    self.get_finalized_custodian_asset(sudt_script_hash)
                        .expect("get custodian asset")
                })
                .borrow_mut();
            *balance = balance.checked_sub(amount).expect("withdrawal overflow");
        }
        // write touched assets to storage
        for (key, balance) in touched_custodian_assets {
            self.set_custodian_asset(key, balance)?;
        }
        Ok(())
    }

    /// Attach block to the rollup main chain
    pub fn attach_block(
        &self,
        block: packed::L2Block,
        rollup_config: &RollupConfig,
    ) -> Result<(), Error> {
        let raw = block.raw();
        let raw_number = raw.number();
        let block_hash = raw.hash();

        // build tx info
        for (index, tx) in block.transactions().into_iter().enumerate() {
            let key = TransactionKey::build_transaction_key(block_hash.pack(), index as u32);
            let info = packed::TransactionInfo::new_builder()
                .key(key)
                .block_number(raw_number.clone())
                .build();
            let tx_hash = tx.hash();
            self.insert_raw(COLUMN_TRANSACTION_INFO, &tx_hash, info.as_slice())?;
        }

        // update finalized custodian assets
        let finality_blocks = rollup_config.finality_blocks().unpack();
        let last_finalized_block_number = raw_number.unpack().saturating_sub(finality_blocks);
        if last_finalized_block_number > 0 {
            let last_finalized_block_hash = self
                .get_block_hash_by_number(last_finalized_block_number)?
                .ok_or_else(|| {
                    Error::from(format!(
                        "last finalized block {} hash not found",
                        last_finalized_block_number
                    ))
                })?;
            let last_finalized_block =
                self.get_block(&last_finalized_block_hash)?.ok_or_else(|| {
                    Error::from(format!(
                        "last finalized block {} not found",
                        last_finalized_block_number
                    ))
                })?;

            let deposit_assets = self
                .get_block_deposit_requests(&last_finalized_block_hash)?
                .expect("finalized deposits")
                .into_iter()
                .map(|deposit| CustodianChange {
                    sudt_script_hash: deposit.sudt_script_hash().unpack(),
                    amount: deposit.amount().unpack(),
                    capacity: deposit.capacity().unpack(),
                });
            let withdrawal_assets = {
                let last_finalized_withdrawals = last_finalized_block.withdrawals().into_iter();
                last_finalized_withdrawals.map(|withdrawal| {
                    let raw = withdrawal.raw();
                    CustodianChange {
                        sudt_script_hash: raw.sudt_script_hash().unpack(),
                        amount: raw.amount().unpack(),
                        capacity: raw.capacity().unpack(),
                    }
                })
            };
            self.update_finalized_custodian_assets(deposit_assets, withdrawal_assets)?;
        }

        // build main chain index
        self.insert_raw(COLUMN_INDEX, raw_number.as_slice(), &block_hash)?;
        self.insert_raw(COLUMN_INDEX, &block_hash, raw_number.as_slice())?;

        // update block tree
        let mut block_smt = self.block_smt()?;
        block_smt
            .update(raw.smt_key().into(), raw.hash().into())
            .map_err(|err| Error::from(format!("SMT error {}", err)))?;
        let root = block_smt.root();
        self.set_block_smt_root(*root)?;

        // update tip
        self.insert_raw(COLUMN_META, &META_TIP_BLOCK_HASH_KEY, &block_hash)?;
        self.prune_block_state_record(raw_number.unpack())?;
        self.set_last_valid_tip_block_hash(&block_hash.into())?;

        Ok(())
    }

    pub fn detach_block(
        &self,
        block: &packed::L2Block,
        rollup_config: &RollupConfig,
    ) -> Result<(), Error> {
        // remove transaction info
        for tx in block.transactions().into_iter() {
            let tx_hash = tx.hash();
            self.delete(COLUMN_TRANSACTION_INFO, &tx_hash)?;
        }

        let block_hash: H256 = block.hash().into();

        // update finalized custodian assets
        let finality_blocks = rollup_config.finality_blocks().unpack();
        let last_finalized_block_number = {
            let block_number = block.raw().number().unpack();
            block_number.saturating_sub(finality_blocks)
        };
        if last_finalized_block_number > 0 {
            let last_finalized_block_hash = self
                .get_block_hash_by_number(last_finalized_block_number)?
                .ok_or_else(|| {
                    Error::from(format!(
                        "last finalized block {} hash not found",
                        last_finalized_block_number
                    ))
                })?;
            let last_finalized_block =
                self.get_block(&last_finalized_block_hash)?.ok_or_else(|| {
                    Error::from(format!(
                        "last finalized block {} not found",
                        last_finalized_block_number
                    ))
                })?;

            let deposit_assets = self
                .get_block_deposit_requests(&last_finalized_block_hash)?
                .expect("finalized deposits")
                .into_iter()
                .map(|deposit| CustodianChange {
                    sudt_script_hash: deposit.sudt_script_hash().unpack(),
                    amount: deposit.amount().unpack(),
                    capacity: deposit.capacity().unpack(),
                });
            let withdrawal_assets = {
                let last_finalized_withdrawals = last_finalized_block.withdrawals().into_iter();
                last_finalized_withdrawals.map(|withdrawal| {
                    let raw = withdrawal.raw();
                    CustodianChange {
                        sudt_script_hash: raw.sudt_script_hash().unpack(),
                        amount: raw.amount().unpack(),
                        capacity: raw.capacity().unpack(),
                    }
                })
            };
            self.update_finalized_custodian_assets(withdrawal_assets, deposit_assets)?;
        }

        let block_number = block.raw().number();
        self.delete(COLUMN_INDEX, block_number.as_slice())?;
        self.delete(COLUMN_INDEX, block_hash.as_slice())?;

        // update block tree
        let mut block_smt = self.block_smt()?;
        block_smt
            .update(block.smt_key().into(), H256::zero())
            .map_err(|err| Error::from(format!("SMT error {}", err)))?;
        let root = block_smt.root();
        self.set_block_smt_root(*root)?;

        // update tip
        let block_number: u64 = block_number.unpack();
        let parent_number = block_number.saturating_sub(1);
        let parent_block_hash = self
            .get_block_hash_by_number(parent_number)?
            .expect("parent block hash");
        self.insert_raw(
            COLUMN_META,
            &META_TIP_BLOCK_HASH_KEY,
            parent_block_hash.as_slice(),
        )?;
        self.set_last_valid_tip_block_hash(&parent_block_hash)?;

        // clear block state
        self.clear_block_state(block_number)?;

        // reset account root
        let block_prev_merkle_state = block.raw().prev_account();
        self.set_account_count(block_prev_merkle_state.count().unpack())?;
        self.set_account_smt_root(block_prev_merkle_state.merkle_root().unpack())?;

        Ok(())
    }

    pub fn record_block_state(
        &self,
        block_number: u64,
        tx_index: u32,
        col: Col,
        raw_key: &[u8],
    ) -> Result<(), Error> {
        let record_key = BlockStateRecordKey::new(block_number, tx_index, col, raw_key);
        self.insert_raw(COLUMN_BLOCK_STATE_RECORD, record_key.as_slice(), &[])
    }

    fn prune_block_state_record(&self, current_block_number: u64) -> Result<(), Error> {
        if current_block_number <= NUMBER_OF_CONFIRMATION {
            return Ok(());
        }
        let to_be_pruned_block_number = current_block_number - NUMBER_OF_CONFIRMATION - 1;
        if to_be_pruned_block_number == 0 {
            return Ok(());
        }
        self.clear_block_state_record(to_be_pruned_block_number)
    }

    pub(crate) fn clear_block_state_record(&self, block_number: u64) -> Result<(), Error> {
        let iter = self.iter_block_state_record(block_number);
        for record_key in iter {
            self.delete(COLUMN_BLOCK_STATE_RECORD, record_key.as_slice())?;
        }
        Ok(())
    }

    pub(crate) fn clear_block_state(&self, block_number: u64) -> Result<(), Error> {
        let iter = self.iter_block_state_record(block_number);
        for record_key in iter {
            let column = record_key.get_column();
            self.delete(column, record_key.state_key())?;
            self.delete(COLUMN_BLOCK_STATE_RECORD, record_key.as_slice())?;
        }
        Ok(())
    }

    fn iter_block_state_record(
        &self,
        block_number: u64,
    ) -> impl Iterator<Item = BlockStateRecordKey> + '_ {
        let start_key = BlockStateRecordKey::new(block_number, 0u32, 0u8, &[]);
        self.get_iter(
            COLUMN_BLOCK_STATE_RECORD,
            IteratorMode::From(start_key.as_slice(), Forward),
        )
        .map(|(key, _value)| BlockStateRecordKey::from_vec(key.to_vec()))
        .take_while(move |key| key.is_same_block(block_number))
    }
}

struct CustodianChange {
    capacity: u64,
    sudt_script_hash: H256,
    amount: u128,
}

// block_number(8 bytes) | tx_index(4 bytes) | col (1 byte) | key (n bytes)
struct BlockStateRecordKey(Vec<u8>);

impl BlockStateRecordKey {
    fn new(block_number: u64, tx_index: u32, col: Col, key: &[u8]) -> Self {
        let mut record_key = Vec::new();
        record_key.resize(13 + key.len(), 0);
        record_key[..8].copy_from_slice(&block_number.to_be_bytes());
        record_key[8..12].copy_from_slice(&tx_index.to_be_bytes());
        record_key[12] = col;
        record_key[13..].copy_from_slice(key);
        BlockStateRecordKey(record_key)
    }

    fn state_key(&self) -> &[u8] {
        &self.0[13..]
    }

    fn from_vec(record_key: Vec<u8>) -> Self {
        BlockStateRecordKey(record_key)
    }

    fn get_column(&self) -> u8 {
        self.0[12]
    }

    fn is_same_block(&self, block_number: u64) -> bool {
        self.0[..8] == block_number.to_be_bytes()
    }

    fn as_slice(&self) -> &[u8] {
        self.0.as_slice()
    }
}
