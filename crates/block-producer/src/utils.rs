#![allow(clippy::clippy::mutable_key_type)]

use crate::debugger;
use crate::types::InputCellInfo;
use crate::{rpc_client::RPCClient, transaction_skeleton::TransactionSkeleton};
use anyhow::{anyhow, Result};
use async_jsonrpc_client::Output;
use gw_common::{blake2b::new_blake2b, H256};
use gw_types::{
    core::DepType,
    packed::{Block, CellDep, CellInput, CellOutput, Header, OutPoint, Script, Transaction},
    prelude::*,
};
use serde::de::DeserializeOwned;
use serde_json::from_value;
use std::path::Path;

// convert json output to result
pub fn to_result<T: DeserializeOwned>(output: Output) -> Result<T> {
    match output {
        Output::Success(success) => Ok(from_value(success.result)?),
        Output::Failure(failure) => Err(anyhow!("JSONRPC error: {}", failure.error)),
    }
}

/// Calculate tx fee
/// TODO accept fee rate args
fn calculate_required_tx_fee(tx_size: usize) -> u64 {
    // tx_size * KB / MIN_FEE_RATE
    tx_size as u64
}

/// Add fee cell to tx skeleton
pub async fn fill_tx_fee(
    tx_skeleton: &mut TransactionSkeleton,
    rpc_client: &RPCClient,
    lock_script: Script,
) -> Result<()> {
    const CHANGE_CELL_CAPACITY: u64 = 61_00000000;

    let estimate_tx_size_with_change = |tx_skeleton: &mut TransactionSkeleton| -> Result<usize> {
        let change_cell = CellOutput::new_builder()
            .lock(lock_script.clone())
            .capacity(CHANGE_CELL_CAPACITY.pack())
            .build();

        tx_skeleton
            .outputs_mut()
            .push((change_cell, Default::default()));

        let tx_size = tx_skeleton.tx_in_block_size()?;
        tx_skeleton.outputs_mut().pop();

        Ok(tx_size)
    };

    // calculate required fee
    // NOTE: Poa will insert a owner cell to inputs if there isn't one in ```fill_poa()```,
    // so most of time, paid_fee should already cover tx_fee. The first thing we need to do
    // is try to generate a change output cell.
    let tx_size = estimate_tx_size_with_change(tx_skeleton)?;
    let tx_fee = calculate_required_tx_fee(tx_size);
    let max_paid_fee = tx_skeleton
        .calculate_fee()?
        .saturating_sub(CHANGE_CELL_CAPACITY);

    let mut required_fee = tx_fee.saturating_sub(max_paid_fee);
    if 0 == required_fee {
        let change_capacity = max_paid_fee + CHANGE_CELL_CAPACITY - tx_fee;
        let change_cell = CellOutput::new_builder()
            .lock(lock_script.clone())
            .capacity(change_capacity.pack())
            .build();

        tx_skeleton
            .outputs_mut()
            .push((change_cell, Default::default()));

        return Ok(());
    }

    required_fee += CHANGE_CELL_CAPACITY;

    let mut change_capacity = 0;
    while required_fee > 0 {
        // to filter used input cells
        let taken_outpoints = tx_skeleton.taken_outpoints()?;
        // get payment cells
        let cells = rpc_client
            .query_payment_cells(lock_script.clone(), required_fee, &taken_outpoints)
            .await?;
        assert!(!cells.is_empty(), "need cells to pay fee");

        // put cells in tx skeleton
        tx_skeleton
            .inputs_mut()
            .extend(cells.into_iter().map(|cell| {
                let input = CellInput::new_builder()
                    .previous_output(cell.out_point.clone())
                    .build();
                InputCellInfo { input, cell }
            }));

        let tx_size = estimate_tx_size_with_change(tx_skeleton)?;
        let tx_fee = calculate_required_tx_fee(tx_size);
        let max_paid_fee = tx_skeleton
            .calculate_fee()?
            .saturating_sub(CHANGE_CELL_CAPACITY);

        required_fee = tx_fee.saturating_sub(max_paid_fee);
        change_capacity = max_paid_fee + CHANGE_CELL_CAPACITY - tx_fee;
    }

    let change_cell = CellOutput::new_builder()
        .lock(lock_script)
        .capacity(change_capacity.pack())
        .build();

    tx_skeleton
        .outputs_mut()
        .push((change_cell, Default::default()));

    Ok(())
}

#[derive(Debug, Clone)]
pub struct CKBGenesisInfo {
    header: Header,
    out_points: Vec<Vec<OutPoint>>,
    sighash_data_hash: H256,
    sighash_type_hash: H256,
    multisig_data_hash: H256,
    multisig_type_hash: H256,
    dao_data_hash: H256,
    dao_type_hash: H256,
}

impl CKBGenesisInfo {
    // Special cells in genesis transactions: (transaction-index, output-index)
    pub const SIGHASH_OUTPUT_LOC: (usize, usize) = (0, 1);
    pub const MULTISIG_OUTPUT_LOC: (usize, usize) = (0, 4);
    pub const DAO_OUTPUT_LOC: (usize, usize) = (0, 2);
    pub const SIGHASH_GROUP_OUTPUT_LOC: (usize, usize) = (1, 0);
    pub const MULTISIG_GROUP_OUTPUT_LOC: (usize, usize) = (1, 1);

    pub fn from_block(genesis_block: &Block) -> Result<Self> {
        let raw_header = genesis_block.header().raw();
        let number: u64 = raw_header.number().unpack();
        if number != 0 {
            return Err(anyhow!("Invalid genesis block number: {}", number));
        }

        let mut sighash_data_hash = None;
        let mut sighash_type_hash = None;
        let mut multisig_data_hash = None;
        let mut multisig_type_hash = None;
        let mut dao_data_hash = None;
        let mut dao_type_hash = None;
        let out_points = genesis_block
            .transactions()
            .into_iter()
            .enumerate()
            .map(|(tx_index, tx)| {
                let raw_tx = tx.raw();
                raw_tx
                    .outputs()
                    .into_iter()
                    .zip(raw_tx.outputs_data().into_iter())
                    .enumerate()
                    .map(|(index, (output, data))| {
                        let data_hash: H256 = {
                            let mut hasher = new_blake2b();
                            hasher.update(&data.raw_data());
                            let mut hash = [0u8; 32];
                            hasher.finalize(&mut hash);
                            hash.into()
                        };
                        if tx_index == Self::SIGHASH_OUTPUT_LOC.0
                            && index == Self::SIGHASH_OUTPUT_LOC.1
                        {
                            sighash_type_hash =
                                output.type_().to_opt().map(|script| script.hash().into());
                            sighash_data_hash = Some(data_hash);
                        }
                        if tx_index == Self::MULTISIG_OUTPUT_LOC.0
                            && index == Self::MULTISIG_OUTPUT_LOC.1
                        {
                            multisig_type_hash =
                                output.type_().to_opt().map(|script| script.hash().into());
                            multisig_data_hash = Some(data_hash);
                        }
                        if tx_index == Self::DAO_OUTPUT_LOC.0 && index == Self::DAO_OUTPUT_LOC.1 {
                            dao_type_hash =
                                output.type_().to_opt().map(|script| script.hash().into());
                            dao_data_hash = Some(data_hash);
                        }
                        let tx_hash = {
                            let mut hasher = new_blake2b();
                            hasher.update(tx.raw().as_slice());
                            let mut hash = [0u8; 32];
                            hasher.finalize(&mut hash);
                            hash
                        };
                        OutPoint::new_builder()
                            .tx_hash(tx_hash.pack())
                            .index((index as u32).pack())
                            .build()
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        let sighash_data_hash =
            sighash_data_hash.ok_or_else(|| anyhow!("No data hash(sighash) found in txs[0][1]"))?;
        let sighash_type_hash =
            sighash_type_hash.ok_or_else(|| anyhow!("No type hash(sighash) found in txs[0][1]"))?;
        let multisig_data_hash = multisig_data_hash
            .ok_or_else(|| anyhow!("No data hash(multisig) found in txs[0][4]"))?;
        let multisig_type_hash = multisig_type_hash
            .ok_or_else(|| anyhow!("No type hash(multisig) found in txs[0][4]"))?;
        let dao_data_hash =
            dao_data_hash.ok_or_else(|| anyhow!("No data hash(dao) found in txs[0][2]"))?;
        let dao_type_hash =
            dao_type_hash.ok_or_else(|| anyhow!("No type hash(dao) found in txs[0][2]"))?;
        Ok(CKBGenesisInfo {
            header: genesis_block.header(),
            out_points,
            sighash_data_hash,
            sighash_type_hash,
            multisig_data_hash,
            multisig_type_hash,
            dao_data_hash,
            dao_type_hash,
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn sighash_data_hash(&self) -> &H256 {
        &self.sighash_data_hash
    }

    pub fn sighash_type_hash(&self) -> &H256 {
        &self.sighash_type_hash
    }

    pub fn multisig_data_hash(&self) -> &H256 {
        &self.multisig_data_hash
    }

    pub fn multisig_type_hash(&self) -> &H256 {
        &self.multisig_type_hash
    }

    pub fn dao_data_hash(&self) -> &H256 {
        &self.dao_data_hash
    }

    pub fn dao_type_hash(&self) -> &H256 {
        &self.dao_type_hash
    }

    pub fn sighash_dep(&self) -> CellDep {
        CellDep::new_builder()
            .out_point(
                self.out_points[Self::SIGHASH_GROUP_OUTPUT_LOC.0][Self::SIGHASH_GROUP_OUTPUT_LOC.1]
                    .clone(),
            )
            .dep_type(DepType::DepGroup.into())
            .build()
    }

    pub fn multisig_dep(&self) -> CellDep {
        CellDep::new_builder()
            .out_point(
                self.out_points[Self::MULTISIG_GROUP_OUTPUT_LOC.0]
                    [Self::MULTISIG_GROUP_OUTPUT_LOC.1]
                    .clone(),
            )
            .dep_type(DepType::DepGroup.into())
            .build()
    }

    pub fn dao_dep(&self) -> CellDep {
        CellDep::new_builder()
            .out_point(self.out_points[Self::DAO_OUTPUT_LOC.0][Self::DAO_OUTPUT_LOC.1].clone())
            .build()
    }
}

pub fn is_debug_env_var_set() -> bool {
    match std::env::var("GODWOKEN_DEBUG") {
        Ok(s) => s.to_lowercase().trim() == "true",
        _ => false,
    }
}

pub async fn dry_run_transaction(rpc_client: &RPCClient, tx: Transaction, action: &str) {
    if is_debug_env_var_set() {
        let dry_run_result = rpc_client.dry_run_transaction(tx.clone()).await;
        match dry_run_result {
            Ok(cycles) => log::info!(
                "Tx({}) {} execution cycles: {}",
                action,
                hex::encode(tx.hash()),
                cycles
            ),
            Err(err) => log::error!(
                "Fail to dry run transaction {}, error: {}",
                hex::encode(tx.hash()),
                err
            ),
        }
    }
}

pub async fn dump_transaction<P: AsRef<Path>>(dir: P, rpc_client: &RPCClient, tx: Transaction) {
    if let Err(err) = debugger::dump_transaction(dir, rpc_client, tx.clone()).await {
        log::error!(
            "Faild to dump transaction {} error: {}",
            hex::encode(&tx.hash()),
            err
        );
    }
}
