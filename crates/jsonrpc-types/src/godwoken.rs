use crate::blockchain::Script;
use anyhow::{anyhow, Error as JsonError};
use ckb_fixed_hash::H256;
use ckb_jsonrpc_types::{JsonBytes, Uint128, Uint32, Uint64};
use gw_types::{bytes::Bytes, offchain, packed, prelude::*};
use serde::{Deserialize, Serialize};
use std::convert::{TryFrom, TryInto};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub struct RawL2Transaction {
    pub from_id: Uint32,
    pub to_id: Uint32,
    pub nonce: Uint32,
    pub args: JsonBytes,
}

impl From<RawL2Transaction> for packed::RawL2Transaction {
    fn from(tx: RawL2Transaction) -> Self {
        let RawL2Transaction {
            from_id,
            to_id,
            nonce,
            args,
        } = tx;
        let args: Bytes = args.into_bytes();
        packed::RawL2Transaction::new_builder()
            .from_id(u32::from(from_id).pack())
            .to_id(u32::from(to_id).pack())
            .nonce(u32::from(nonce).pack())
            .args(args.pack())
            .build()
    }
}

impl From<packed::RawL2Transaction> for RawL2Transaction {
    fn from(raw_l2_transaction: packed::RawL2Transaction) -> RawL2Transaction {
        let from_id: u32 = raw_l2_transaction.from_id().unpack();
        let to_id: u32 = raw_l2_transaction.to_id().unpack();
        let nonce: u32 = raw_l2_transaction.nonce().unpack();
        Self {
            from_id: from_id.into(),
            to_id: to_id.into(),
            nonce: nonce.into(),
            args: JsonBytes::from_bytes(raw_l2_transaction.args().unpack()),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub struct L2Transaction {
    pub raw: RawL2Transaction,
    pub signature: JsonBytes,
}

impl From<L2Transaction> for packed::L2Transaction {
    fn from(tx: L2Transaction) -> Self {
        let L2Transaction { raw, signature } = tx;

        packed::L2Transaction::new_builder()
            .raw(raw.into())
            .signature(signature.into_bytes().pack())
            .build()
    }
}

impl From<packed::L2Transaction> for L2Transaction {
    fn from(l2_transaction: packed::L2Transaction) -> L2Transaction {
        Self {
            raw: l2_transaction.raw().into(),
            signature: JsonBytes::from_bytes(l2_transaction.signature().unpack()),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub struct L2TransactionView {
    #[serde(flatten)]
    pub inner: L2Transaction,
    pub hash: H256,
}

impl From<packed::L2Transaction> for L2TransactionView {
    fn from(l2_tx: packed::L2Transaction) -> L2TransactionView {
        let hash = H256::from(l2_tx.raw().hash());
        let inner = L2Transaction::from(l2_tx);
        L2TransactionView { inner, hash }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub struct LogItem {
    pub account_id: Uint32,
    // The actual type is `u8`
    pub service_flag: Uint32,
    pub data: JsonBytes,
}

impl From<LogItem> for packed::LogItem {
    fn from(json: LogItem) -> packed::LogItem {
        let LogItem {
            account_id,
            service_flag,
            data,
        } = json;
        packed::LogItem::new_builder()
            .account_id(account_id.value().pack())
            .service_flag((service_flag.value() as u8).into())
            .data(data.into_bytes().pack())
            .build()
    }
}

impl From<packed::LogItem> for LogItem {
    fn from(data: packed::LogItem) -> LogItem {
        let account_id: u32 = data.account_id().unpack();
        let service_flag: u8 = data.service_flag().into();
        let data = JsonBytes::from_bytes(data.data().unpack());
        LogItem {
            account_id: Uint32::from(account_id),
            service_flag: Uint32::from(service_flag as u32),
            data,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub struct TxReceipt {
    pub tx_witness_hash: H256,
    pub post_state: AccountMerkleState,
    pub read_data_hashes: Vec<H256>,
    pub logs: Vec<LogItem>,
}

impl From<TxReceipt> for packed::TxReceipt {
    fn from(json: TxReceipt) -> packed::TxReceipt {
        let TxReceipt {
            tx_witness_hash,
            post_state,
            read_data_hashes,
            logs,
        } = json;
        let tx_witness_hash: [u8; 32] = tx_witness_hash.into();
        let read_data_hashes: Vec<_> = read_data_hashes
            .into_iter()
            .map(|hash| {
                let hash: [u8; 32] = hash.into();
                hash.pack()
            })
            .collect();
        let logs: Vec<packed::LogItem> = logs.into_iter().map(|item| item.into()).collect();
        packed::TxReceipt::new_builder()
            .tx_witness_hash(tx_witness_hash.pack())
            .post_state(post_state.into())
            .read_data_hashes(read_data_hashes.pack())
            .logs(logs.pack())
            .build()
    }
}

impl From<packed::TxReceipt> for TxReceipt {
    fn from(data: packed::TxReceipt) -> TxReceipt {
        let tx_witness_hash: [u8; 32] = data.tx_witness_hash().unpack();
        let post_state: AccountMerkleState = data.post_state().into();
        let read_data_hashes: Vec<_> = data
            .read_data_hashes()
            .into_iter()
            .map(|hash| {
                let hash: [u8; 32] = hash.unpack();
                hash.into()
            })
            .collect();
        let logs: Vec<LogItem> = data.logs().into_iter().map(|item| item.into()).collect();
        TxReceipt {
            tx_witness_hash: tx_witness_hash.into(),
            post_state,
            read_data_hashes,
            logs,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ChallengeTargetType {
    Transaction,
    Withdrawal,
}

impl Default for ChallengeTargetType {
    fn default() -> Self {
        Self::Transaction
    }
}

impl From<ChallengeTargetType> for packed::Byte {
    fn from(json: ChallengeTargetType) -> packed::Byte {
        match json {
            ChallengeTargetType::Transaction => packed::Byte::new(0),
            ChallengeTargetType::Withdrawal => packed::Byte::new(1),
        }
    }
}
impl TryFrom<packed::Byte> for ChallengeTargetType {
    type Error = JsonError;

    fn try_from(v: packed::Byte) -> Result<ChallengeTargetType, Self::Error> {
        match u8::from(v) {
            0 => Ok(ChallengeTargetType::Transaction),
            1 => Ok(ChallengeTargetType::Withdrawal),
            _ => Err(anyhow!("Invalid challenge target type {}", v)),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub struct ChallengeTarget {
    pub block_hash: H256,                 // hash of challenged block
    pub target_index: Uint32,             // target index
    pub target_type: ChallengeTargetType, // target type
}

impl From<ChallengeTarget> for packed::ChallengeTarget {
    fn from(json: ChallengeTarget) -> packed::ChallengeTarget {
        let ChallengeTarget {
            block_hash,
            target_index,
            target_type,
        } = json;
        packed::ChallengeTarget::new_builder()
            .block_hash(block_hash.pack())
            .target_index(u32::from(target_index).pack())
            .target_type(target_type.into())
            .build()
    }
}

impl From<packed::ChallengeTarget> for ChallengeTarget {
    fn from(challenge_target: packed::ChallengeTarget) -> ChallengeTarget {
        let target_index: u32 = challenge_target.target_index().unpack();
        let target_type: packed::Byte = challenge_target.target_type();
        Self {
            block_hash: challenge_target.block_hash().unpack(),
            target_index: Uint32::from(target_index),
            target_type: target_type.try_into().expect("invalid target type"),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub struct ChallengeWitness {
    pub raw_l2block: RawL2Block,
    pub block_proof: JsonBytes, // block proof
}

impl From<ChallengeWitness> for packed::ChallengeWitness {
    fn from(json: ChallengeWitness) -> packed::ChallengeWitness {
        let ChallengeWitness {
            raw_l2block,
            block_proof,
        } = json;
        let raw_l2block: packed::RawL2Block = raw_l2block.into();
        packed::ChallengeWitness::new_builder()
            .raw_l2block(raw_l2block)
            .block_proof(block_proof.into_bytes().pack())
            .build()
    }
}

impl From<packed::ChallengeWitness> for ChallengeWitness {
    fn from(data: packed::ChallengeWitness) -> ChallengeWitness {
        let raw_l2block: RawL2Block = data.raw_l2block().into();
        let block_proof = JsonBytes::from_bytes(data.block_proof().unpack());
        Self {
            raw_l2block,
            block_proof,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub struct L2Block {
    pub raw: RawL2Block,
    pub kv_state: Vec<KVPair>,
    pub kv_state_proof: JsonBytes,
    pub transactions: Vec<L2Transaction>,
    pub block_proof: JsonBytes,
    pub withdrawals: Vec<WithdrawalRequest>,
}

impl From<L2Block> for packed::L2Block {
    fn from(json: L2Block) -> packed::L2Block {
        let L2Block {
            raw,
            kv_state,
            kv_state_proof,
            transactions,
            block_proof,
            withdrawals,
        } = json;
        let kv_pair_vec: Vec<packed::KVPair> = kv_state.into_iter().map(|k| k.into()).collect();
        let packed_kv_state = packed::KVPairVec::new_builder().set(kv_pair_vec).build();
        let transaction_vec: Vec<packed::L2Transaction> =
            transactions.into_iter().map(|t| t.into()).collect();
        let packed_transactions = packed::L2TransactionVec::new_builder()
            .set(transaction_vec)
            .build();
        let withdrawal_requests_vec: Vec<packed::WithdrawalRequest> =
            withdrawals.into_iter().map(|w| w.into()).collect();
        let packed_withdrawal_requests = packed::WithdrawalRequestVec::new_builder()
            .set(withdrawal_requests_vec)
            .build();
        packed::L2Block::new_builder()
            .raw(raw.into())
            .kv_state(packed_kv_state)
            .kv_state_proof(kv_state_proof.into_bytes().pack())
            .transactions(packed_transactions)
            .block_proof(block_proof.into_bytes().pack())
            .withdrawals(packed_withdrawal_requests)
            .build()
    }
}

impl From<packed::L2Block> for L2Block {
    fn from(l2_block: packed::L2Block) -> L2Block {
        Self {
            raw: l2_block.raw().into(),
            kv_state: l2_block.kv_state().into_iter().map(|k| k.into()).collect(),
            kv_state_proof: JsonBytes::from_bytes(l2_block.kv_state_proof().unpack()),
            transactions: l2_block
                .transactions()
                .into_iter()
                .map(|t| t.into())
                .collect(),
            block_proof: JsonBytes::from_bytes(l2_block.block_proof().unpack()),
            withdrawals: l2_block
                .withdrawals()
                .into_iter()
                .map(|w| w.into())
                .collect(),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub struct RawL2Block {
    pub number: Uint64,
    pub parent_block_hash: H256,
    pub block_producer_id: Uint32,
    pub stake_cell_owner_lock_hash: H256,
    pub timestamp: Uint64,
    pub prev_account: AccountMerkleState,
    pub post_account: AccountMerkleState,
    pub submit_transactions: SubmitTransactions,
    pub submit_withdrawals: SubmitWithdrawals,
    // hash(account_root | account_count) of each withdrawals & transactions
    pub state_checkpoint_list: Vec<H256>,
}

impl From<RawL2Block> for packed::RawL2Block {
    fn from(json: RawL2Block) -> packed::RawL2Block {
        let RawL2Block {
            number,
            parent_block_hash,
            block_producer_id,
            stake_cell_owner_lock_hash,
            timestamp,
            prev_account,
            post_account,
            submit_transactions,
            submit_withdrawals,
            state_checkpoint_list,
        } = json;

        let state_checkpoint_list = state_checkpoint_list
            .into_iter()
            .map(|checkpoint| checkpoint.pack())
            .pack();
        packed::RawL2Block::new_builder()
            .number(u64::from(number).pack())
            .parent_block_hash(parent_block_hash.pack())
            .block_producer_id(u32::from(block_producer_id).pack())
            .stake_cell_owner_lock_hash(stake_cell_owner_lock_hash.pack())
            .timestamp(u64::from(timestamp).pack())
            .prev_account(prev_account.into())
            .post_account(post_account.into())
            .submit_transactions(submit_transactions.into())
            .submit_withdrawals(submit_withdrawals.into())
            .state_checkpoint_list(state_checkpoint_list)
            .build()
    }
}

impl From<packed::RawL2Block> for RawL2Block {
    fn from(raw_l2_block: packed::RawL2Block) -> RawL2Block {
        let number: u64 = raw_l2_block.number().unpack();
        let block_producer_id: u32 = raw_l2_block.block_producer_id().unpack();
        let timestamp: u64 = raw_l2_block.timestamp().unpack();
        let state_checkpoint_list = raw_l2_block
            .state_checkpoint_list()
            .into_iter()
            .map(|checkpoint| checkpoint.unpack())
            .collect();
        Self {
            number: number.into(),
            parent_block_hash: raw_l2_block.parent_block_hash().unpack(),
            block_producer_id: block_producer_id.into(),
            stake_cell_owner_lock_hash: raw_l2_block.stake_cell_owner_lock_hash().unpack(),
            timestamp: timestamp.into(),
            prev_account: raw_l2_block.prev_account().into(),
            post_account: raw_l2_block.post_account().into(),
            submit_transactions: raw_l2_block.submit_transactions().into(),
            submit_withdrawals: raw_l2_block.submit_withdrawals().into(),
            state_checkpoint_list,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub struct L2BlockView {
    pub raw: RawL2Block,
    pub kv_state: Vec<KVPair>,
    pub kv_state_proof: JsonBytes,
    pub transactions: Vec<L2TransactionView>,
    pub block_proof: JsonBytes,
    pub withdrawal_requests: Vec<WithdrawalRequest>,
    pub hash: H256,
}

impl From<packed::L2Block> for L2BlockView {
    fn from(l2_block: packed::L2Block) -> L2BlockView {
        Self {
            hash: H256::from(l2_block.raw().hash()),
            raw: l2_block.raw().into(),
            kv_state: l2_block.kv_state().into_iter().map(|k| k.into()).collect(),
            kv_state_proof: JsonBytes::from_bytes(l2_block.kv_state_proof().unpack()),
            transactions: l2_block
                .transactions()
                .into_iter()
                .map(|t| t.into())
                .collect(),
            block_proof: JsonBytes::from_bytes(l2_block.block_proof().unpack()),
            withdrawal_requests: l2_block
                .withdrawals()
                .into_iter()
                .map(|w| w.into())
                .collect(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub enum L2BlockStatus {
    Unfinalized,
    Finalized,
    Reverted,
}

impl Default for L2BlockStatus {
    fn default() -> Self {
        Self::Unfinalized
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub struct L2BlockWithStatus {
    pub l2_block_view: L2BlockView,
    pub l2_block_status: L2BlockStatus,
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub struct SubmitTransactions {
    pub tx_witness_root: H256,
    pub tx_count: Uint32,
    // hash(account_root | account_count) before apply all transactions
    pub prev_state_checkpoint: H256,
}

impl From<SubmitTransactions> for packed::SubmitTransactions {
    fn from(json: SubmitTransactions) -> packed::SubmitTransactions {
        let SubmitTransactions {
            tx_witness_root,
            tx_count,
            prev_state_checkpoint,
        } = json;
        packed::SubmitTransactions::new_builder()
            .tx_witness_root(tx_witness_root.pack())
            .tx_count(u32::from(tx_count).pack())
            .prev_state_checkpoint(prev_state_checkpoint.pack())
            .build()
    }
}

impl From<packed::SubmitTransactions> for SubmitTransactions {
    fn from(submit_transactions: packed::SubmitTransactions) -> SubmitTransactions {
        let tx_count: u32 = submit_transactions.tx_count().unpack();
        Self {
            tx_witness_root: submit_transactions.tx_witness_root().unpack(),
            tx_count: tx_count.into(),
            prev_state_checkpoint: submit_transactions.prev_state_checkpoint().unpack(),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub struct SubmitWithdrawals {
    pub withdrawal_witness_root: H256,
    pub withdrawal_count: Uint32,
}

impl From<SubmitWithdrawals> for packed::SubmitWithdrawals {
    fn from(json: SubmitWithdrawals) -> packed::SubmitWithdrawals {
        let SubmitWithdrawals {
            withdrawal_witness_root,
            withdrawal_count,
        } = json;
        packed::SubmitWithdrawals::new_builder()
            .withdrawal_witness_root(withdrawal_witness_root.pack())
            .withdrawal_count(u32::from(withdrawal_count).pack())
            .build()
    }
}

impl From<packed::SubmitWithdrawals> for SubmitWithdrawals {
    fn from(data: packed::SubmitWithdrawals) -> SubmitWithdrawals {
        let withdrawal_count: u32 = data.withdrawal_count().unpack();
        Self {
            withdrawal_witness_root: data.withdrawal_witness_root().unpack(),
            withdrawal_count: withdrawal_count.into(),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub struct AccountMerkleState {
    pub merkle_root: H256,
    pub count: Uint32,
}

impl From<AccountMerkleState> for packed::AccountMerkleState {
    fn from(json: AccountMerkleState) -> packed::AccountMerkleState {
        let AccountMerkleState { merkle_root, count } = json;
        packed::AccountMerkleState::new_builder()
            .merkle_root(merkle_root.pack())
            .count(u32::from(count).pack())
            .build()
    }
}

impl From<packed::AccountMerkleState> for AccountMerkleState {
    fn from(account_merkel_state: packed::AccountMerkleState) -> AccountMerkleState {
        let count: u32 = account_merkel_state.count().unpack();
        Self {
            merkle_root: account_merkel_state.merkle_root().unpack(),
            count: count.into(),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub struct BlockMerkleState {
    pub merkle_root: H256,
    pub count: Uint64,
}

impl From<BlockMerkleState> for packed::BlockMerkleState {
    fn from(json: BlockMerkleState) -> packed::BlockMerkleState {
        let count: u64 = json.count.into();
        packed::BlockMerkleState::new_builder()
            .merkle_root(json.merkle_root.pack())
            .count(count.pack())
            .build()
    }
}

impl From<packed::BlockMerkleState> for BlockMerkleState {
    fn from(block_merkle_state: packed::BlockMerkleState) -> BlockMerkleState {
        let count: u64 = block_merkle_state.count().unpack();
        Self {
            merkle_root: block_merkle_state.merkle_root().unpack(),
            count: count.into(),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub struct KVPair {
    pub k: H256,
    pub v: H256,
}

impl From<KVPair> for packed::KVPair {
    fn from(json: KVPair) -> packed::KVPair {
        let KVPair { k, v } = json;
        packed::KVPair::new_builder()
            .k(k.pack())
            .v(v.pack())
            .build()
    }
}

impl From<packed::KVPair> for KVPair {
    fn from(kvpair: packed::KVPair) -> KVPair {
        Self {
            k: kvpair.k().unpack(),
            v: kvpair.v().unpack(),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub struct GlobalState {
    pub account: AccountMerkleState,
    pub block: BlockMerkleState,
    pub reverted_block_root: H256,
    pub last_finalized_block_number: Uint64,
    pub status: Uint32,
}

impl From<GlobalState> for packed::GlobalState {
    fn from(json: GlobalState) -> packed::GlobalState {
        let GlobalState {
            account,
            block,
            reverted_block_root,
            last_finalized_block_number,
            status,
        } = json;
        let last_finalized_block_number: u64 = last_finalized_block_number.into();
        let status: u32 = status.into();
        packed::GlobalState::new_builder()
            .account(account.into())
            .block(block.into())
            .reverted_block_root(reverted_block_root.pack())
            .last_finalized_block_number(last_finalized_block_number.pack())
            .status((status as u8).into())
            .build()
    }
}
impl From<packed::GlobalState> for GlobalState {
    fn from(global_state: packed::GlobalState) -> GlobalState {
        let last_finalized_block_number: u64 = global_state.last_finalized_block_number().unpack();
        let status: u8 = global_state.status().into();
        Self {
            account: global_state.account().into(),
            block: global_state.block().into(),
            reverted_block_root: global_state.reverted_block_root().unpack(),
            last_finalized_block_number: last_finalized_block_number.into(),
            status: (status as u32).into(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub struct DepositRequest {
    pub script: Script,
    pub sudt_script_hash: H256,
    pub amount: Uint128,
    pub capacity: Uint64,
}

impl From<DepositRequest> for packed::DepositRequest {
    fn from(json: DepositRequest) -> packed::DepositRequest {
        let DepositRequest {
            script,
            sudt_script_hash,
            amount,
            capacity,
        } = json;
        packed::DepositRequest::new_builder()
            .script(script.into())
            .sudt_script_hash(sudt_script_hash.pack())
            .amount(u128::from(amount).pack())
            .capacity(u64::from(capacity).pack())
            .build()
    }
}

impl From<packed::DepositRequest> for DepositRequest {
    fn from(deposit_request: packed::DepositRequest) -> DepositRequest {
        let amount: u128 = deposit_request.amount().unpack();
        let capacity: u64 = deposit_request.capacity().unpack();
        Self {
            script: deposit_request.script().into(),
            sudt_script_hash: deposit_request.sudt_script_hash().unpack(),
            amount: amount.into(),
            capacity: capacity.into(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub struct WithdrawalRequest {
    pub raw: RawWithdrawalRequest,
    pub signature: JsonBytes,
}

impl From<WithdrawalRequest> for packed::WithdrawalRequest {
    fn from(json: WithdrawalRequest) -> packed::WithdrawalRequest {
        let WithdrawalRequest { raw, signature } = json;
        packed::WithdrawalRequest::new_builder()
            .raw(raw.into())
            .signature(signature.into_bytes().pack())
            .build()
    }
}

impl From<packed::WithdrawalRequest> for WithdrawalRequest {
    fn from(withdrawal_request: packed::WithdrawalRequest) -> WithdrawalRequest {
        Self {
            raw: withdrawal_request.raw().into(),
            signature: JsonBytes::from_bytes(withdrawal_request.signature().unpack()),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub struct RawWithdrawalRequest {
    pub nonce: Uint32,
    pub capacity: Uint64,
    pub amount: Uint128,
    // buyer can pay sell_amount and sell_capacity to unlock
    pub sell_amount: Uint128,
    pub sell_capacity: Uint64,
    pub sudt_script_hash: H256,
    pub account_script_hash: H256,
    // layer1 lock to withdraw after challenge period
    pub owner_lock_hash: H256,
    // layer1 lock to receive the payment, must exists on the chain
    pub payment_lock_hash: H256,
}

impl From<RawWithdrawalRequest> for packed::RawWithdrawalRequest {
    fn from(json: RawWithdrawalRequest) -> packed::RawWithdrawalRequest {
        let RawWithdrawalRequest {
            nonce,
            capacity,
            amount,
            sell_amount,
            sell_capacity,
            sudt_script_hash,
            account_script_hash,
            owner_lock_hash,
            payment_lock_hash,
        } = json;
        packed::RawWithdrawalRequest::new_builder()
            .nonce(u32::from(nonce).pack())
            .capacity(u64::from(capacity).pack())
            .amount(u128::from(amount).pack())
            .sell_amount(u128::from(sell_amount).pack())
            .sell_capacity(u64::from(sell_capacity).pack())
            .sudt_script_hash(sudt_script_hash.pack())
            .account_script_hash(account_script_hash.pack())
            .owner_lock_hash(owner_lock_hash.pack())
            .payment_lock_hash(payment_lock_hash.pack())
            .build()
    }
}

impl From<packed::RawWithdrawalRequest> for RawWithdrawalRequest {
    fn from(raw_withdrawal_request: packed::RawWithdrawalRequest) -> RawWithdrawalRequest {
        let nonce: u32 = raw_withdrawal_request.nonce().unpack();
        let capacity: u64 = raw_withdrawal_request.capacity().unpack();
        let amount: u128 = raw_withdrawal_request.amount().unpack();
        let sell_capacity: u64 = raw_withdrawal_request.sell_capacity().unpack();
        let sell_amount: u128 = raw_withdrawal_request.sell_amount().unpack();
        Self {
            nonce: nonce.into(),
            capacity: capacity.into(),
            amount: amount.into(),
            sell_capacity: sell_capacity.into(),
            sell_amount: sell_amount.into(),
            sudt_script_hash: raw_withdrawal_request.sudt_script_hash().unpack(),
            account_script_hash: raw_withdrawal_request.account_script_hash().unpack(),
            owner_lock_hash: raw_withdrawal_request.owner_lock_hash().unpack(),
            payment_lock_hash: raw_withdrawal_request.payment_lock_hash().unpack(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub struct L2BlockCommittedInfo {
    pub number: Uint64,
    pub block_hash: H256,
    pub transaction_hash: H256,
}

impl From<L2BlockCommittedInfo> for packed::L2BlockCommittedInfo {
    fn from(json: L2BlockCommittedInfo) -> packed::L2BlockCommittedInfo {
        let L2BlockCommittedInfo {
            number,
            block_hash,
            transaction_hash,
        } = json;
        packed::L2BlockCommittedInfo::new_builder()
            .number(u64::from(number).pack())
            .block_hash(block_hash.pack())
            .transaction_hash(transaction_hash.pack())
            .build()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub struct RollupConfig {
    pub l1_sudt_script_type_hash: H256,
    pub custodian_script_type_hash: H256,
    pub deposit_script_type_hash: H256,
    pub withdrawal_script_type_hash: H256,
    pub challenge_script_type_hash: H256,
    pub stake_script_type_hash: H256,
    pub l2_sudt_validator_script_type_hash: H256,
    pub burn_lock_hash: H256,
    pub required_staking_capacity: Uint64,
    pub challenge_maturity_blocks: Uint64,
    pub finality_blocks: Uint64,
    pub reward_burn_rate: Uint32,           // * reward_burn_rate / 100
    pub allowed_eoa_type_hashes: Vec<H256>, // list of script code_hash allowed an EOA(external owned account) to use
    pub allowed_contract_type_hashes: Vec<H256>, // list of script code_hash allowed a contract account to use
}

impl From<RollupConfig> for packed::RollupConfig {
    fn from(json: RollupConfig) -> packed::RollupConfig {
        let RollupConfig {
            l1_sudt_script_type_hash,
            custodian_script_type_hash,
            deposit_script_type_hash,
            withdrawal_script_type_hash,
            challenge_script_type_hash,
            stake_script_type_hash,
            l2_sudt_validator_script_type_hash,
            burn_lock_hash,
            required_staking_capacity,
            challenge_maturity_blocks,
            finality_blocks,
            reward_burn_rate,             // * reward_burn_rate / 100
            allowed_eoa_type_hashes, // list of script code_hash allowed an EOA(external owned account) to use
            allowed_contract_type_hashes, // list of script code_hash allowed a contract account to use
        } = json;
        let required_staking_capacity: u64 = required_staking_capacity.into();
        let challenge_maturity_blocks: u64 = challenge_maturity_blocks.into();
        let finality_blocks: u64 = finality_blocks.into();
        let reward_burn_rate: u32 = reward_burn_rate.into();
        let reward_burn_rate: u8 = reward_burn_rate.try_into().expect("reward burn rate");
        packed::RollupConfig::new_builder()
            .l1_sudt_script_type_hash(l1_sudt_script_type_hash.pack())
            .custodian_script_type_hash(custodian_script_type_hash.pack())
            .deposit_script_type_hash(deposit_script_type_hash.pack())
            .withdrawal_script_type_hash(withdrawal_script_type_hash.pack())
            .challenge_script_type_hash(challenge_script_type_hash.pack())
            .stake_script_type_hash(stake_script_type_hash.pack())
            .l2_sudt_validator_script_type_hash(l2_sudt_validator_script_type_hash.pack())
            .burn_lock_hash(burn_lock_hash.pack())
            .required_staking_capacity(required_staking_capacity.pack())
            .challenge_maturity_blocks(challenge_maturity_blocks.pack())
            .finality_blocks(finality_blocks.pack())
            .reward_burn_rate(reward_burn_rate.into())
            .allowed_eoa_type_hashes(
                allowed_eoa_type_hashes
                    .into_iter()
                    .map(|hash| hash.pack())
                    .pack(),
            )
            .allowed_contract_type_hashes(
                allowed_contract_type_hashes
                    .into_iter()
                    .map(|hash| hash.pack())
                    .pack(),
            )
            .build()
    }
}

impl From<packed::RollupConfig> for RollupConfig {
    fn from(data: packed::RollupConfig) -> RollupConfig {
        let required_staking_capacity: u64 = data.required_staking_capacity().unpack();
        let challenge_maturity_blocks: u64 = data.challenge_maturity_blocks().unpack();
        let finality_blocks: u64 = data.finality_blocks().unpack();
        let reward_burn_date: u8 = data.reward_burn_rate().into();
        RollupConfig {
            l1_sudt_script_type_hash: data.l1_sudt_script_type_hash().unpack(),
            custodian_script_type_hash: data.custodian_script_type_hash().unpack(),
            deposit_script_type_hash: data.deposit_script_type_hash().unpack(),
            withdrawal_script_type_hash: data.withdrawal_script_type_hash().unpack(),
            challenge_script_type_hash: data.challenge_script_type_hash().unpack(),
            stake_script_type_hash: data.stake_script_type_hash().unpack(),
            l2_sudt_validator_script_type_hash: data.l2_sudt_validator_script_type_hash().unpack(),
            burn_lock_hash: data.burn_lock_hash().unpack(),
            required_staking_capacity: required_staking_capacity.into(),
            challenge_maturity_blocks: challenge_maturity_blocks.into(),
            finality_blocks: finality_blocks.into(),
            reward_burn_rate: (reward_burn_date as u32).into(),
            allowed_eoa_type_hashes: data
                .allowed_eoa_type_hashes()
                .into_iter()
                .map(|hash| hash.unpack())
                .collect(),
            allowed_contract_type_hashes: data
                .allowed_contract_type_hashes()
                .into_iter()
                .map(|hash| hash.unpack())
                .collect(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub struct RunResult {
    // return data
    pub return_data: JsonBytes,
    // log data
    pub logs: Vec<LogItem>,
}

impl From<offchain::RunResult> for RunResult {
    fn from(data: offchain::RunResult) -> RunResult {
        let offchain::RunResult {
            return_data, logs, ..
        } = data;
        RunResult {
            return_data: JsonBytes::from_vec(return_data),
            logs: logs.into_iter().map(Into::into).collect(),
        }
    }
}
