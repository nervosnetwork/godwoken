#![allow(clippy::clippy::mutable_key_type)]

use crate::{
    poa::{PoA, ShouldIssueBlock},
    produce_block::{produce_block, ProduceBlockParam, ProduceBlockResult},
    rpc_client::{DepositInfo, RPCClient},
    test_mode_control::TestModeControl,
    transaction_skeleton::TransactionSkeleton,
    types::ChainEvent,
    types::{CellInfo, InputCellInfo},
    utils::{self, fill_tx_fee, CKBGenesisInfo},
    wallet::Wallet,
};
use anyhow::{anyhow, Context, Result};
use ckb_types::prelude::Unpack as CKBUnpack;
use futures::{future::select_all, FutureExt};
use gw_chain::chain::{Chain, SyncEvent};
use gw_common::{h256_ext::H256Ext, CKB_SUDT_SCRIPT_ARGS, H256};
use gw_config::BlockProducerConfig;
use gw_generator::{Generator, RollupContext};
use gw_jsonrpc_types::test_mode::TestModePayload;
use gw_mem_pool::pool::MemPool;
use gw_store::Store;
use gw_types::{
    bytes::Bytes,
    core::{DepType, ScriptHashType, Status},
    packed::{
        Byte32, CellDep, CellInput, CellOutput, CustodianLockArgs, DepositLockArgs, GlobalState,
        L2Block, OutPoint, OutPointVec, RollupAction, RollupActionUnion, RollupSubmitBlock, Script,
        Transaction, WitnessArgs,
    },
    prelude::*,
};
use parking_lot::Mutex;
use std::{
    collections::{HashMap, HashSet},
    convert::TryFrom,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const TRANSACTION_SRIPT_ERROR: &str = "TransactionScriptError";

fn generate_custodian_cells(
    rollup_context: &RollupContext,
    block: &L2Block,
    deposit_cells: &[DepositInfo],
) -> Vec<(CellOutput, Bytes)> {
    let block_hash: Byte32 = block.hash().pack();
    let block_number = block.raw().number();
    deposit_cells
        .iter()
        .map(|deposit_info| {
            let lock_args: Bytes = {
                let deposit_lock_args = {
                    let lock_args: Bytes = deposit_info.cell.output.lock().args().unpack();
                    DepositLockArgs::new_unchecked(lock_args.slice(32..))
                };

                let custodian_lock_args = CustodianLockArgs::new_builder()
                    .deposit_block_hash(block_hash.clone())
                    .deposit_block_number(block_number.clone())
                    .deposit_lock_args(deposit_lock_args)
                    .build();

                let rollup_type_hash = rollup_context.rollup_script_hash.as_slice().iter();
                rollup_type_hash
                    .chain(custodian_lock_args.as_slice().iter())
                    .cloned()
                    .collect()
            };
            let lock = Script::new_builder()
                .code_hash(rollup_context.rollup_config.custodian_script_type_hash())
                .hash_type(ScriptHashType::Type.into())
                .args(lock_args.pack())
                .build();

            // use custodian lock
            let cell = deposit_info
                .cell
                .output
                .clone()
                .as_builder()
                .lock(lock)
                .build();
            let data = deposit_info.cell.data.clone();
            (cell, data)
        })
        .collect()
}

async fn resolve_tx_deps(rpc_client: &RPCClient, tx_hash: [u8; 32]) -> Result<Vec<CellInfo>> {
    async fn resolve_dep_group(rpc_client: &RPCClient, dep: CellDep) -> Result<Vec<CellDep>> {
        // return dep
        if dep.dep_type() == DepType::Code.into() {
            return Ok(vec![dep]);
        }
        // parse dep group
        let cell = rpc_client
            .get_cell(dep.out_point())
            .await?
            .ok_or_else(|| anyhow!("can't find dep group cell"))?;
        let out_points =
            OutPointVec::from_slice(&cell.data).map_err(|_| anyhow!("invalid dep group"))?;
        let cell_deps = out_points
            .into_iter()
            .map(|out_point| {
                CellDep::new_builder()
                    .out_point(out_point)
                    .dep_type(DepType::Code.into())
                    .build()
            })
            .collect();
        Ok(cell_deps)
    }

    // get deposit cells txs
    let tx = rpc_client
        .get_transaction(tx_hash.into())
        .await?
        .ok_or_else(|| anyhow!("can't get deposit tx"))?;
    let mut resolve_dep_futs: Vec<_> = tx
        .raw()
        .cell_deps()
        .into_iter()
        .map(|dep| resolve_dep_group(rpc_client, dep).boxed())
        .collect();
    let mut get_cell_futs = Vec::default();

    // wait resolved dep groups futures
    while !resolve_dep_futs.is_empty() {
        let (tx_cell_deps_res, _index, remained) = select_all(resolve_dep_futs.into_iter()).await;
        resolve_dep_futs = remained;
        let tx_cell_deps = tx_cell_deps_res?;
        let futs = tx_cell_deps
            .iter()
            .map(|dep| rpc_client.get_cell(dep.out_point()).boxed());
        get_cell_futs.extend(futs);
    }

    // wait all cells
    let mut cells = Vec::with_capacity(get_cell_futs.len());
    for cell_fut in get_cell_futs {
        let cell = cell_fut
            .await?
            .ok_or_else(|| anyhow!("can't find dep cell"))?;
        cells.push(cell);
    }
    Ok(cells)
}

pub struct BlockProducer {
    rollup_config_hash: H256,
    store: Store,
    chain: Arc<Mutex<Chain>>,
    mem_pool: Arc<Mutex<MemPool>>,
    generator: Arc<Generator>,
    poa: PoA,
    wallet: Wallet,
    config: BlockProducerConfig,
    rpc_client: RPCClient,
    ckb_genesis_info: CKBGenesisInfo,
    tests_control: Option<TestModeControl>,
}

impl BlockProducer {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        rollup_config_hash: H256,
        store: Store,
        generator: Arc<Generator>,
        chain: Arc<Mutex<Chain>>,
        mem_pool: Arc<Mutex<MemPool>>,
        rpc_client: RPCClient,
        ckb_genesis_info: CKBGenesisInfo,
        config: BlockProducerConfig,
        tests_control: Option<TestModeControl>,
    ) -> Result<Self> {
        let wallet = Wallet::from_config(&config.wallet_config).with_context(|| "init wallet")?;
        let poa = PoA::new(
            rpc_client.clone(),
            wallet.lock_script().clone(),
            config.poa_lock_dep.clone().into(),
            config.poa_state_dep.clone().into(),
        );

        let block_producer = BlockProducer {
            rollup_config_hash,
            store,
            generator,
            chain,
            mem_pool,
            rpc_client,
            wallet,
            poa,
            ckb_genesis_info,
            config,
            tests_control,
        };
        Ok(block_producer)
    }

    pub async fn handle_event(&mut self, event: ChainEvent) -> Result<()> {
        if let Some(ref tests_control) = self.tests_control {
            match tests_control.payload().await {
                Some(TestModePayload::Challenge { .. }) // Payload not match
                | Some(TestModePayload::WaitForChallengeMaturity) // Payload not match
                | None => return Ok(()), // Wait payload
                Some(TestModePayload::None) // Produce block
                | Some(TestModePayload::BadBlock { .. }) => (), // Produce block but create bad block
            }
        }

        let last_sync_event = { self.chain.lock().last_sync_event().to_owned() };
        match last_sync_event {
            SyncEvent::Success => (),
            _ => return Ok(()),
        }

        // assume the chain is updated
        let tip_block = match event {
            ChainEvent::Reverted {
                old_tip: _,
                new_block,
            } => new_block,
            ChainEvent::NewBlock { block } => block,
        };
        let header = tip_block.header();
        let tip_hash: H256 = header.hash().into();

        // query median time & rollup cell
        let rollup_cell_opt = self.rpc_client.query_rollup_cell().await?;
        let rollup_cell = rollup_cell_opt.ok_or_else(|| anyhow!("can't found rollup cell"))?;
        let rollup_state = {
            let global_state = GlobalState::from_slice(&rollup_cell.data)?;
            let status: u8 = global_state.status().into();
            Status::try_from(status).map_err(|n| anyhow!("invalid status {}", n))?
        };
        if Status::Halting == rollup_state {
            return Ok(());
        }

        let median_time = self.rpc_client.get_block_median_time(tip_hash).await?;
        let poa_cell_input = InputCellInfo {
            input: CellInput::new_builder()
                .previous_output(rollup_cell.out_point.clone())
                .build(),
            cell: rollup_cell.clone(),
        };

        // try issue next block
        if let ShouldIssueBlock::Yes = self
            .poa
            .should_issue_next_block(median_time, &poa_cell_input)
            .await?
        {
            self.produce_next_block(median_time, rollup_cell).await?;
        }
        Ok(())
    }

    pub async fn produce_next_block(
        &mut self,
        median_time: Duration,
        rollup_cell: CellInfo,
    ) -> Result<()> {
        if let Some(ref tests_control) = self.tests_control {
            match tests_control.payload().await {
                Some(TestModePayload::None) => tests_control.clear_none().await?,
                Some(TestModePayload::BadBlock { .. }) => (),
                _ => unreachable!(),
            }
        }

        let block_producer_id = self.config.account_id;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("timestamp")
            .as_millis() as u64;

        // get deposit cells
        // check deposit cells again to prevent upstream components errors.
        let deposit_cells =
            self.sanitize_deposit_cells(self.rpc_client.query_deposit_cells().await?);

        // get txs & withdrawal requests from mem pool
        let mut txs = Vec::new();
        let mut withdrawal_requests = Vec::new();
        {
            let mem_pool = self.mem_pool.lock();
            for entry in mem_pool.pending().values() {
                if let Some(withdrawal) = entry.withdrawals.first() {
                    withdrawal_requests.push(withdrawal.clone());
                } else {
                    txs.extend(entry.txs.iter().cloned());
                }
            }
        };
        let parent_block = self.chain.lock().local_state().tip().clone();
        let max_withdrawal_capacity = std::u128::MAX;

        let available_custodians = if withdrawal_requests.is_empty() {
            crate::withdrawal::AvailableCustodians::default()
        } else {
            let db = self.store.begin_transaction();
            let mut sudt_scripts: HashMap<[u8; 32], Script> = HashMap::new();
            let sudt_custodians = {
                let reqs = withdrawal_requests.iter();
                let sudt_reqs = reqs.filter(|req| {
                    let sudt_script_hash: [u8; 32] = req.raw().sudt_script_hash().unpack();
                    0 != req.raw().amount().unpack() && CKB_SUDT_SCRIPT_ARGS != sudt_script_hash
                });

                let to_hash = sudt_reqs.map(|req| req.raw().sudt_script_hash().unpack());
                let has_script = to_hash.filter_map(|hash: [u8; 32]| {
                    if let Some(script) = sudt_scripts.get(&hash).cloned() {
                        return Some((hash, script));
                    }

                    // Try rpc
                    match smol::block_on(
                        self.rpc_client.query_verified_custodian_type_script(&hash),
                    ) {
                        Ok(opt_script) => opt_script.map(|script| {
                            sudt_scripts.insert(hash, script.clone());
                            (hash, script)
                        }),
                        Err(err) => {
                            log::debug!("get custodian type script err {}", err);
                            None
                        }
                    }
                });

                let to_custodian = has_script.filter_map(|(hash, script)| {
                    match db.get_finalized_custodian_asset(hash.into()) {
                        Ok(custodian_balance) => Some((hash, (custodian_balance, script))),
                        Err(err) => {
                            log::warn!("get custodian err {}", err);
                            None
                        }
                    }
                });
                to_custodian.collect::<HashMap<[u8; 32], (u128, Script)>>()
            };

            let ckb_custodian = match db.get_finalized_custodian_asset(CKB_SUDT_SCRIPT_ARGS.into())
            {
                Ok(balance) => balance,
                Err(err) => {
                    log::warn!("get ckb custodian err {}", err);
                    0
                }
            };

            crate::withdrawal::AvailableCustodians {
                capacity: ckb_custodian,
                sudt: sudt_custodians,
            }
        };
        log::debug!("available custodians {:?}", available_custodians);

        // produce block
        let reverted_block_root: H256 = {
            let db = self.store.begin_transaction();
            let smt = db.reverted_block_smt()?;
            smt.root().to_owned()
        };
        let param = ProduceBlockParam {
            db: self.store.begin_transaction(),
            generator: &self.generator,
            block_producer_id,
            stake_cell_owner_lock_hash: self.wallet.lock_script().hash().into(),
            timestamp,
            txs,
            deposit_requests: deposit_cells.iter().map(|d| &d.request).cloned().collect(),
            withdrawal_requests,
            parent_block: &parent_block,
            reverted_block_root,
            rollup_config_hash: &self.rollup_config_hash,
            max_withdrawal_capacity,
            available_custodians,
        };
        let block_result = produce_block(param)?;
        let ProduceBlockResult {
            mut block,
            mut global_state,
            unused_transactions,
            unused_withdrawal_requests,
        } = block_result;
        let number: u64 = block.raw().number().unpack();
        log::info!(
            "produce new block #{} (txs: {}, deposits: {}, withdrawals: {}, staled txs: {}, staled withdrawals: {})",
            number,
            block.transactions().len(),
            deposit_cells.len(),
            block.withdrawals().len(),
            unused_transactions.len(),
            unused_withdrawal_requests.len()
        );

        if let Some(ref tests_control) = self.tests_control {
            if let Some(TestModePayload::BadBlock { .. }) = tests_control.payload().await {
                let (bad_block, bad_global_state) = tests_control
                    .generate_a_bad_block(block, global_state)
                    .await?;

                block = bad_block;
                global_state = bad_global_state;
            }
        }

        // composit tx
        let tx = self
            .complete_tx_skeleton(deposit_cells, block, global_state, median_time, rollup_cell)
            .await?;

        utils::dry_run_transaction(
            &self.rpc_client,
            tx.clone(),
            format!("L2 block {}", number).as_str(),
        )
        .await;

        // send transaction
        match self.rpc_client.send_transaction(tx.clone()).await {
            Ok(tx_hash) => {
                log::info!(
                    "\nSubmitted l2 block {} in tx {}\n",
                    number,
                    hex::encode(tx_hash.as_slice())
                );
            }
            Err(err) => {
                log::error!("Submitting l2 block error: {}", err);
                self.poa.reset_current_round();

                // dumping script error transactions
                if err.to_string().contains(TRANSACTION_SRIPT_ERROR) {
                    // dumping failed tx
                    utils::dump_transaction(
                        &self.config.debug_tx_dump_path,
                        &self.rpc_client,
                        tx.clone(),
                    )
                    .await;
                } else {
                    log::debug!("Skip dumping non-script-error tx");
                }
            }
        }
        Ok(())
    }

    async fn complete_tx_skeleton(
        &self,
        deposit_cells: Vec<DepositInfo>,
        block: L2Block,
        global_state: GlobalState,
        median_time: Duration,
        rollup_cell: CellInfo,
    ) -> Result<Transaction> {
        let rollup_context = self.generator.rollup_context();
        let mut tx_skeleton = TransactionSkeleton::default();
        let rollup_cell_input_index = tx_skeleton.inputs().len();
        // rollup cell
        tx_skeleton.inputs_mut().push(InputCellInfo {
            input: CellInput::new_builder()
                .previous_output(rollup_cell.out_point.clone())
                .build(),
            cell: rollup_cell.clone(),
        });
        // rollup deps
        tx_skeleton
            .cell_deps_mut()
            .push(self.config.rollup_cell_type_dep.clone().into());
        // rollup config cell
        tx_skeleton
            .cell_deps_mut()
            .push(self.config.rollup_config_cell_dep.clone().into());
        // deposit lock dep
        if !deposit_cells.is_empty() {
            let cell_dep: CellDep = self.config.deposit_cell_lock_dep.clone().into();
            tx_skeleton
                .cell_deps_mut()
                .push(CellDep::new_unchecked(cell_dep.as_bytes()));
        }
        // secp256k1 lock, used for unlock tx fee payment cells
        tx_skeleton
            .cell_deps_mut()
            .push(self.ckb_genesis_info.sighash_dep());

        // Package pending revert withdrawals and custodians
        let db = { self.chain.lock().store().begin_transaction() };
        let reverted_block_hashes = db.get_reverted_block_hashes()?;

        let rpc_client = &self.rpc_client;
        let (revert_custodians, mut collected_block_hashes) = rpc_client
            .query_custodian_cells_by_block_hashes(&reverted_block_hashes)
            .await?;
        let (revert_withdrawals, block_hashes) = rpc_client
            .query_withdrawal_cells_by_block_hashes(&reverted_block_hashes)
            .await?;
        collected_block_hashes.extend(block_hashes);

        // rollup action
        let rollup_action = {
            let submit_builder = if !collected_block_hashes.is_empty() {
                let db = self.store.begin_transaction();
                let block_smt = db.reverted_block_smt()?;

                let local_root: &H256 = block_smt.root();
                let global_revert_block_root: H256 = global_state.reverted_block_root().unpack();
                assert_eq!(local_root, &global_revert_block_root);

                let keys: Vec<H256> = collected_block_hashes.into_iter().collect();
                let leaves = keys.iter().map(|hash| (*hash, H256::one()));
                let proof = block_smt
                    .merkle_proof(keys.clone())?
                    .compile(leaves.collect())?;
                for key in keys.iter() {
                    log::info!("submit revert block {:?}", hex::encode(key.as_slice()));
                }

                RollupSubmitBlock::new_builder()
                    .reverted_block_hashes(keys.pack())
                    .reverted_block_proof(proof.0.pack())
            } else {
                RollupSubmitBlock::new_builder()
            };

            let submit_block = submit_builder.block(block.clone()).build();

            RollupAction::new_builder()
                .set(RollupActionUnion::RollupSubmitBlock(submit_block))
                .build()
        };

        // witnesses
        tx_skeleton.witnesses_mut().push(
            WitnessArgs::new_builder()
                .output_type(Some(rollup_action.as_bytes()).pack())
                .build(),
        );
        // output
        let output = rollup_cell.output.clone();
        let output_data = global_state.as_bytes();
        tx_skeleton.outputs_mut().push((output, output_data));
        // deposit cells
        for deposit in &deposit_cells {
            let input = CellInput::new_builder()
                .previous_output(deposit.cell.out_point.clone())
                .build();
            tx_skeleton.inputs_mut().push(InputCellInfo {
                input,
                cell: deposit.cell.clone(),
            });
        }

        // Some deposit cells might have type scripts for sUDTs, handle cell deps
        // here.
        let deposit_type_deps: HashSet<CellDep> = {
            // fetch deposit cells deps
            let dep_cell_futs: Vec<_> = deposit_cells
                .iter()
                .filter_map(|deposit| {
                    deposit.cell.output.type_().to_opt().map(|_type_| {
                        resolve_tx_deps(&self.rpc_client, deposit.cell.out_point.tx_hash().unpack())
                    })
                })
                .collect();

            // wait futures
            let mut dep_cells: Vec<CellInfo> = Vec::new();
            for fut in dep_cell_futs {
                dep_cells.extend(fut.await?);
            }

            // resolve deposit cells deps
            let dep_cell_by_data: HashMap<[u8; 32], OutPoint> = dep_cells
                .iter()
                .map(|cell| {
                    let data_hash =
                        ckb_types::packed::CellOutput::calc_data_hash(&cell.data).unpack();
                    (data_hash, cell.out_point.clone())
                })
                .collect();
            let dep_cell_by_type: HashMap<[u8; 32], OutPoint> = dep_cells
                .iter()
                .filter_map(|cell| {
                    cell.output
                        .type_()
                        .to_opt()
                        .map(|type_| (type_.hash(), cell.out_point.clone()))
                })
                .collect();

            let mut deps: HashSet<CellDep> = Default::default();
            for deposit in &deposit_cells {
                if let Some(type_) = deposit.cell.output.type_().to_opt() {
                    let code_hash: [u8; 32] = type_.code_hash().unpack();
                    let out_point_opt = match ScriptHashType::try_from(type_.hash_type())
                        .map_err(|n| anyhow!("invalid hash_type {}", n))?
                    {
                        ScriptHashType::Data => dep_cell_by_data.get(&code_hash),
                        ScriptHashType::Type => dep_cell_by_type.get(&code_hash),
                    };
                    let out_point = out_point_opt
                        .ok_or_else(|| anyhow!("can't find deps code_hash: {:?}", code_hash))?;
                    let cell_dep = CellDep::new_builder()
                        .out_point(out_point.to_owned())
                        .dep_type(DepType::Code.into())
                        .build();
                    deps.insert(cell_dep);
                }
            }
            deps
        };
        tx_skeleton.cell_deps_mut().extend(deposit_type_deps);
        // custodian cells
        let custodian_cells = generate_custodian_cells(rollup_context, &block, &deposit_cells);
        tx_skeleton.outputs_mut().extend(custodian_cells);
        self.poa
            .fill_poa(&mut tx_skeleton, rollup_cell_input_index, median_time)
            .await?;
        // stake cell
        let generated_stake = crate::stake::generate(
            &rollup_cell,
            rollup_context,
            &block,
            &self.config,
            &self.rpc_client,
            self.wallet.lock_script().to_owned(),
        )
        .await?;
        tx_skeleton.cell_deps_mut().extend(generated_stake.deps);
        tx_skeleton.inputs_mut().extend(generated_stake.inputs);
        tx_skeleton
            .outputs_mut()
            .push((generated_stake.output, generated_stake.output_data));

        // withdrawal cells
        if let Some(generated_withdrawal_cells) = crate::withdrawal::generate(
            &rollup_cell,
            rollup_context,
            &block,
            &self.config,
            &self.rpc_client,
        )
        .await?
        {
            tx_skeleton
                .cell_deps_mut()
                .extend(generated_withdrawal_cells.deps);
            tx_skeleton
                .inputs_mut()
                .extend(generated_withdrawal_cells.inputs);
            tx_skeleton
                .outputs_mut()
                .extend(generated_withdrawal_cells.outputs);
        }

        if let Some(reverted_deposits) =
            crate::deposit::revert(&rollup_context, &self.config, revert_custodians)?
        {
            log::info!("reverted deposits {}", reverted_deposits.inputs.len());

            tx_skeleton.cell_deps_mut().extend(reverted_deposits.deps);

            let input_len = tx_skeleton.inputs().len();
            let witness_len = tx_skeleton.witnesses_mut().len();
            if input_len != witness_len {
                // append dummy witness args to align our reverted deposit witness args
                let dummy_witness_argses = (0..input_len - witness_len)
                    .into_iter()
                    .map(|_| WitnessArgs::default())
                    .collect::<Vec<_>>();
                tx_skeleton.witnesses_mut().extend(dummy_witness_argses);
            }

            tx_skeleton.inputs_mut().extend(reverted_deposits.inputs);
            tx_skeleton
                .witnesses_mut()
                .extend(reverted_deposits.witness_args);
            tx_skeleton.outputs_mut().extend(reverted_deposits.outputs);
        }

        // reverted withdrawal cells
        if let Some(reverted_withdrawals) =
            crate::withdrawal::revert(rollup_context, &self.config, revert_withdrawals)?
        {
            log::info!("reverted withdrawals {}", reverted_withdrawals.inputs.len());

            tx_skeleton
                .cell_deps_mut()
                .extend(reverted_withdrawals.deps);

            let input_len = tx_skeleton.inputs().len();
            let witness_len = tx_skeleton.witnesses_mut().len();
            if input_len != witness_len {
                // append dummy witness args to align our reverted withdrawal witness args
                let dummy_witness_argses = (0..input_len - witness_len)
                    .into_iter()
                    .map(|_| WitnessArgs::default())
                    .collect::<Vec<_>>();
                tx_skeleton.witnesses_mut().extend(dummy_witness_argses);
            }

            tx_skeleton.inputs_mut().extend(reverted_withdrawals.inputs);
            tx_skeleton
                .witnesses_mut()
                .extend(reverted_withdrawals.witness_args);
            tx_skeleton
                .outputs_mut()
                .extend(reverted_withdrawals.outputs);
        }

        // check cell dep duplicate (deposits, withdrawals, reverted_withdrawals sudt type dep)
        {
            let deps: HashSet<_> = tx_skeleton.cell_deps_mut().iter().collect();
            *tx_skeleton.cell_deps_mut() = deps.into_iter().cloned().collect();
        }

        // tx fee cell
        fill_tx_fee(
            &mut tx_skeleton,
            &self.rpc_client,
            self.wallet.lock_script().to_owned(),
        )
        .await?;
        debug_assert_eq!(
            tx_skeleton.taken_outpoints()?.len(),
            tx_skeleton.inputs().len(),
            "check duplicated inputs"
        );
        // sign
        let tx = self.wallet.sign_tx_skeleton(tx_skeleton)?;
        log::debug!("final tx size: {}", tx.as_slice().len());
        Ok(tx)
    }

    // check deposit cells again to prevent upstream components errors.
    fn sanitize_deposit_cells(&self, unsanitize_deposits: Vec<DepositInfo>) -> Vec<DepositInfo> {
        let mut deposit_cells = Vec::with_capacity(unsanitize_deposits.len());
        for cell in unsanitize_deposits {
            // check deposit lock
            // the lock should be correct unless the upstream ckb-indexer has bugs
            if let Err(err) = self.check_deposit_cell(&cell) {
                log::debug!("[sanitize deposit cell] {}", err);
                continue;
            }
            deposit_cells.push(cell);
        }
        deposit_cells
    }

    // check deposit cell
    fn check_deposit_cell(&self, cell: &DepositInfo) -> Result<()> {
        let ctx = self.generator.rollup_context();
        let hash_type = ScriptHashType::Type.into();
        // check deposit lock
        // the lock should be correct unless the upstream ckb-indexer has bugs
        {
            let lock = cell.cell.output.lock();
            if lock.code_hash() != ctx.rollup_config.deposit_script_type_hash()
                || lock.hash_type() != hash_type
            {
                return Err(anyhow!(
                    "Invalid deposit lock, expect code_hash: {}, hash_type: Type, got: {}, {}",
                    ctx.rollup_config.deposit_script_type_hash(),
                    lock.code_hash(),
                    lock.hash_type()
                ));
            }
            let args: Bytes = lock.args().unpack();
            if args.len() < 32 {
                return Err(anyhow!(
                    "Invalid deposit args, expect len: 32, got: {}",
                    args.len()
                ));
            }
            if &args[..32] != ctx.rollup_script_hash.as_slice() {
                return Err(anyhow!(
                    "Invalid deposit args, expect rollup_script_hash: {}, got: {}",
                    hex::encode(ctx.rollup_script_hash.as_slice()),
                    hex::encode(&args[..32])
                ));
            }
        }

        // check sUDT
        // sUDT may be invalid, this may caused by malicious user
        if let Some(type_) = cell.cell.output.type_().to_opt() {
            if type_.code_hash() != ctx.rollup_config.l1_sudt_script_type_hash()
                || type_.hash_type() != hash_type
            {
                return Err(anyhow!(
                    "Invalid deposit sUDT, expect code_hash: {}, hash_type: Type, got: {}, {}",
                    ctx.rollup_config.l1_sudt_script_type_hash(),
                    type_.code_hash(),
                    type_.hash_type()
                ));
            }
        }

        // check request
        // request deposit account maybe invalid, this may caused by malicious user
        {
            let script = cell.request.script();
            if script.hash_type() != ScriptHashType::Type.into() {
                return Err(anyhow!(
                    "Invalid deposit account script: unexpected hash_type: Data"
                ));
            }
            if ctx
                .rollup_config
                .allowed_eoa_type_hashes()
                .into_iter()
                .all(|type_hash| script.code_hash() != type_hash)
            {
                return Err(anyhow!(
                    "Invalid deposit account script: unknown code_hash: {:?}",
                    hex::encode(script.code_hash().as_slice())
                ));
            }
            let args: Bytes = script.args().unpack();
            if args.len() < 32 {
                return Err(anyhow!(
                    "Invalid deposit account args, expect len: 32, got: {}",
                    args.len()
                ));
            }
            if &args[..32] != ctx.rollup_script_hash.as_slice() {
                return Err(anyhow!(
                    "Invalid deposit account args, expect rollup_script_hash: {}, got: {}",
                    hex::encode(ctx.rollup_script_hash.as_slice()),
                    hex::encode(&args[..32])
                ));
            }
            Ok(())
        }
    }
}
