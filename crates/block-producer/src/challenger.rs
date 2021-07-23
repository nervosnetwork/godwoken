#![allow(clippy::mutable_key_type)]

use crate::cleaner::{Cleaner, VerifierCell};
use crate::poa::{PoA, ShouldIssueBlock};
use crate::rpc_client::RPCClient;
use crate::test_mode_control::TestModeControl;
use crate::transaction_skeleton::TransactionSkeleton;
use crate::types::{CellInfo, ChainEvent, InputCellInfo, TxStatus};
use crate::utils::{self, fill_tx_fee, CKBGenesisInfo};
use crate::wallet::Wallet;

use anyhow::{anyhow, Result};
use ckb_types::prelude::{Builder, Entity};
use gw_chain::chain::{Chain, ChallengeCell, SyncEvent};
use gw_chain::challenge::{RevertContext, VerifyContext};
use gw_common::H256;
use gw_config::BlockProducerConfig;
use gw_generator::{ChallengeContext, RollupContext};
use gw_jsonrpc_types::test_mode::TestModePayload;
use gw_types::bytes::Bytes;
use gw_types::core::{ChallengeTargetType, Status};
use gw_types::packed::{
    CellDep, CellInput, CellOutput, GlobalState, OutPoint, Script, Transaction, WitnessArgs,
};
use gw_types::prelude::{Pack, Unpack};
use smol::lock::Mutex;

use std::collections::HashSet;
use std::convert::TryFrom;
use std::sync::Arc;
use std::time::{Duration, Instant};

mod cancel_challenge;
mod enter_challenge;
mod revert;

use cancel_challenge::CancelChallengeOutput;
use enter_challenge::EnterChallenge;
use revert::Revert;

pub struct Challenger {
    rollup_context: RollupContext,
    rpc_client: RPCClient,
    wallet: Wallet,
    config: BlockProducerConfig,
    ckb_genesis_info: CKBGenesisInfo,
    chain: Arc<parking_lot::Mutex<Chain>>,
    poa: Arc<Mutex<PoA>>,
    tests_control: Option<TestModeControl>,
    cleaner: Arc<Cleaner>,
}

impl Challenger {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rollup_context: RollupContext,
        rpc_client: RPCClient,
        wallet: Wallet,
        config: BlockProducerConfig,
        ckb_genesis_info: CKBGenesisInfo,
        chain: Arc<parking_lot::Mutex<Chain>>,
        poa: Arc<Mutex<PoA>>,
        tests_control: Option<TestModeControl>,
        cleaner: Arc<Cleaner>,
    ) -> Self {
        Self {
            rollup_context,
            rpc_client,
            wallet,
            config,
            ckb_genesis_info,
            poa,
            chain,
            tests_control,
            cleaner,
        }
    }

    pub async fn handle_event(&mut self, event: ChainEvent) -> Result<()> {
        if let Some(ref tests_control) = self.tests_control {
            match tests_control.payload().await {
                Some(TestModePayload::Challenge { .. })
                    | Some(TestModePayload::WaitForChallengeMaturity)
                    | Some(TestModePayload::None) => (),
                    Some(TestModePayload::BadBlock { .. }) // Payload not match (BadBlock for block producer)
                        | None => return Ok(()), // Wait payload
            }
        }

        let tip_hash = to_tip_hash(&event);
        let median_time = self.rpc_client.get_block_median_time(tip_hash).await?;
        let rollup = RollupState::query(&self.rpc_client).await?;

        {
            let mut poa = self.poa.lock().await;
            let rollup_input = rollup.rollup_input();
            let check_lock = poa.should_issue_next_block(median_time, &rollup_input);
            if ShouldIssueBlock::Yes != check_lock.await? {
                return Ok(());
            }
        }

        if let Some(ref tests_control) = self.tests_control {
            if let Some(TestModePayload::Challenge { .. }) = tests_control.payload().await {
                match rollup.status()? {
                    Status::Halting => return Ok(()), // Already halting, do nothing, we can't challenge block when rollup is halted
                    Status::Running => {
                        let context = tests_control.challenge().await?;
                        return self.challenge_block(rollup, context, median_time).await;
                    }
                };
            }
        }

        let last_sync_event = { self.chain.lock().last_sync_event().to_owned() };
        log::debug!("load chain last sync event {:?}", last_sync_event);

        match last_sync_event {
            SyncEvent::Success => Ok(()),
            SyncEvent::BadBlock { context } => {
                if let Some(ref tests_control) = self.tests_control {
                    match tests_control.payload().await {
                        Some(TestModePayload::WaitForChallengeMaturity) => return Ok(()), // do nothing
                        Some(TestModePayload::None) => tests_control.clear_none().await?,
                        _ => unreachable!(),
                    }
                }
                {
                    let hash = hex::encode::<[u8; 32]>(context.target.block_hash().unpack());
                    let idx: u32 = context.target.target_index().unpack();
                    let type_ = ChallengeTargetType::try_from(context.target.target_type())
                        .map_err(|_| anyhow!("invalid challenge type"))?;
                    log::info!("challenge block 0x{} target {} type {:?}", hash, idx, type_);
                }
                self.challenge_block(rollup, context, median_time).await
            }
            SyncEvent::BadChallenge { cell, context } => {
                if let Some(ref tests_control) = self.tests_control {
                    match tests_control.payload().await {
                        Some(TestModePayload::WaitForChallengeMaturity) => return Ok(()), // do nothing
                        Some(TestModePayload::None) => tests_control.clear_none().await?,
                        _ => unreachable!(),
                    }
                }
                self.cancel_challenge(rollup, cell, context, median_time)
                    .await
            }
            SyncEvent::WaitChallenge { cell, context } => {
                if let Some(ref tests_control) = self.tests_control {
                    match tests_control.payload().await {
                        Some(TestModePayload::WaitForChallengeMaturity) => {
                            tests_control
                                .wait_for_challenge_maturity(rollup.status()?)
                                .await?
                        }
                        Some(TestModePayload::None) => tests_control.clear_none().await?,
                        _ => unreachable!(),
                    }
                }
                let tip_number = to_tip_number(&event);
                self.revert(rollup, cell, context, tip_number, median_time)
                    .await
            }
        }
    }

    async fn challenge_block(
        &self,
        rollup_state: RollupState,
        context: ChallengeContext,
        media_time: Duration,
    ) -> Result<()> {
        if Status::Halting == rollup_state.status()? {
            // Already entered challenge
            return Ok(());
        }

        let block_numer = context.witness.raw_l2block().number().unpack();
        let rewards_lock = {
            let challenger_config = &self.config.challenger_config;
            challenger_config.rewards_receiver_lock.clone().into()
        };
        let prev_state = rollup_state.get_state().to_owned();
        let enter_challenge =
            EnterChallenge::new(prev_state, &self.rollup_context, context, rewards_lock);
        let challenge_output = enter_challenge.build_output();

        // Build challenge transaction
        let mut tx_skeleton = TransactionSkeleton::default();

        // Rollup
        let rollup_deps = vec![
            self.config.rollup_cell_type_dep.clone().into(),
            self.config.rollup_config_cell_dep.clone().into(),
        ];
        let rollup_output = (
            rollup_state.rollup_output(),
            challenge_output.post_global_state.as_bytes(),
        );
        let rollup_witness = challenge_output.rollup_witness;

        tx_skeleton.cell_deps_mut().extend(rollup_deps);
        tx_skeleton.inputs_mut().push(rollup_state.rollup_input());
        tx_skeleton.outputs_mut().push(rollup_output);
        tx_skeleton.witnesses_mut().push(rollup_witness);

        // Poa
        {
            let poa = self.poa.lock().await;
            poa.fill_poa(&mut tx_skeleton, 0, media_time).await?;
        }

        // Challenge
        let challenge_cell = challenge_output.challenge_cell;
        tx_skeleton.outputs_mut().push(challenge_cell);

        let challenger_lock_dep = self.ckb_genesis_info.sighash_dep();
        let challenger_lock = self.wallet.lock_script().to_owned();
        tx_skeleton.cell_deps_mut().push(challenger_lock_dep);
        fill_tx_fee(&mut tx_skeleton, &self.rpc_client, challenger_lock).await?;

        let tx = self.wallet.sign_tx_skeleton(tx_skeleton)?;

        utils::dry_run_transaction(&self.rpc_client, tx.clone(), "challenge block").await;
        utils::dump_transaction(
            &self.config.debug_tx_dump_path,
            &self.rpc_client,
            tx.clone(),
        )
        .await;

        let tx_hash = self.rpc_client.send_transaction(tx).await?;
        log::info!("Challenge block {} in tx {}", block_numer, to_hex(&tx_hash));
        Ok(())
    }

    async fn cancel_challenge(
        &self,
        rollup_state: RollupState,
        challenge_cell: ChallengeCell,
        context: VerifyContext,
        media_time: Duration,
    ) -> Result<()> {
        if Status::Running == rollup_state.status()? {
            // Already cancelled
            return Ok(());
        }

        let challenge_cell = to_cell_info(challenge_cell);
        let prev_state = rollup_state.get_state().to_owned();
        let owner_lock = self.wallet.lock_script().to_owned();
        let cancel_output =
            cancel_challenge::build_output(&self.rollup_context, prev_state, owner_lock, context)?;

        // Build verifier transaction
        let verifier_cell = cancel_output.verifier_cell.clone();
        let tx = self.build_verifier_tx(verifier_cell).await?;
        let verifier_spent_inputs = extract_inputs(&tx);
        let verifier_tx_hash = self.rpc_client.send_transaction(tx).await?;
        log::info!("Create verifier in tx {}", to_hex(&verifier_tx_hash));

        self.wait_tx_proposed(verifier_tx_hash).await?;

        // Build cancellation transaction
        let challenge_input = to_input_cell_info(challenge_cell);
        let verifier_context = {
            let cell_dep = cancel_output.verifier_dep(&self.config)?;
            let input = cancel_output.verifier_input(verifier_tx_hash, 0);
            let witness = cancel_output.verifier_witness.clone();
            VerifierContext::new(cell_dep, input, witness, Some(verifier_spent_inputs))
        };

        let tx = self
            .build_cancel_tx(
                rollup_state,
                cancel_output,
                challenge_input,
                verifier_context.clone(),
                media_time,
            )
            .await?;

        utils::dry_run_transaction(&self.rpc_client, tx.clone(), "cancel challenge").await;
        utils::dump_transaction(
            &self.config.debug_tx_dump_path,
            &self.rpc_client,
            tx.clone(),
        )
        .await;

        let verifier_cell = VerifierCell::new(
            verifier_context.cell_dep,
            verifier_context.input,
            verifier_context.witness,
        );
        match self.rpc_client.send_transaction(tx).await {
            Ok(tx_hash) => {
                self.cleaner.watch_verifier(verifier_cell, Some(tx_hash));
                log::info!("Cancel challenge in tx {}", to_hex(&tx_hash));
            }
            Err(err) => {
                self.cleaner.watch_verifier(verifier_cell, None);
                log::warn!("Cancel challenge failed {}", err);
            }
        }

        Ok(())
    }

    async fn revert(
        &self,
        rollup_state: RollupState,
        challenge_cell: ChallengeCell,
        context: RevertContext,
        tip_block_number: u64,
        media_time: Duration,
    ) -> Result<()> {
        if Status::Running == rollup_state.status()? {
            // Already reverted
            return Ok(());
        }

        // Check challenge maturity
        let challenge_maturity_blocks: u64 = {
            let config = &self.rollup_context.rollup_config;
            config.challenge_maturity_blocks().unpack()
        };
        let challenge_cell = to_cell_info(challenge_cell);
        let challenge_tx_block_number = {
            let tx_hash: H256 = challenge_cell.out_point.tx_hash().unpack();
            let tx_status = self.rpc_client.get_transaction_status(tx_hash).await?;
            if !matches!(tx_status, Some(TxStatus::Committed)) {
                log::debug!("challenge tx isn't committed");
                return Ok(());
            }

            let query = self.rpc_client.get_transaction_block_number(tx_hash).await;
            query?.ok_or_else(|| anyhow!("challenge tx block number not found"))?
        };

        const FLAG_SINCE_RELATIVE: u64 =
            0b1000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000;
        const FLAG_SINCE_BLOCK_NUMBER: u64 =
            0b000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000_0000;
        let since = {
            let block_number = ckb_types::core::BlockNumber::from_le_bytes(
                challenge_maturity_blocks.to_le_bytes(),
            );
            FLAG_SINCE_RELATIVE | FLAG_SINCE_BLOCK_NUMBER | block_number
        };

        if tip_block_number.saturating_sub(challenge_tx_block_number) < challenge_maturity_blocks {
            return Ok(());
        }

        // Collect stake cells
        let stake_owner_lock_hashes = {
            let blocks = context.revert_witness.reverted_blocks.clone().into_iter();
            blocks.map(|b| b.stake_cell_owner_lock_hash().unpack())
        };
        let stake_cells = {
            let rpc_client = &self.rpc_client;
            let query = rpc_client.query_stake_cells_by_owner_lock_hashes(stake_owner_lock_hashes);
            query.await?
        };
        let prev_state = rollup_state.get_state().to_owned();
        let burn_lock = self.config.challenger_config.burn_lock.clone().into();

        let revert = Revert::new(
            &self.rollup_context,
            prev_state,
            &challenge_cell,
            &stake_cells,
            burn_lock,
            context,
        );
        let revert_output = revert.build_output()?;

        // Build revert transaction
        let mut tx_skeleton = TransactionSkeleton::default();

        // Rollup
        let rollup_deps = vec![
            self.config.rollup_cell_type_dep.clone().into(),
            self.config.rollup_config_cell_dep.clone().into(),
        ];
        let rollup_output = (
            rollup_state.rollup_output(),
            revert_output.post_global_state.as_bytes(),
        );
        let rollup_witness = revert_output.rollup_witness;

        tx_skeleton.cell_deps_mut().extend(rollup_deps);
        tx_skeleton.inputs_mut().push(rollup_state.rollup_input());
        tx_skeleton.outputs_mut().push(rollup_output);
        tx_skeleton.witnesses_mut().push(rollup_witness);

        // Poa
        {
            let poa = self.poa.lock().await;
            poa.fill_poa(&mut tx_skeleton, 0, media_time).await?;
        }

        // Challenge
        let challenge_input = to_input_cell_info_with_since(challenge_cell, since);
        let challenge_dep = self.config.challenge_cell_lock_dep.clone().into();
        tx_skeleton.cell_deps_mut().push(challenge_dep);
        tx_skeleton.inputs_mut().push(challenge_input);

        // Stake
        let stake_inputs = stake_cells.into_iter().map(to_input_cell_info);
        let stake_dep = self.config.stake_cell_lock_dep.clone().into();
        tx_skeleton.cell_deps_mut().push(stake_dep);
        tx_skeleton.inputs_mut().extend(stake_inputs);

        // Rewards
        tx_skeleton.outputs_mut().extend(revert_output.reward_cells);

        // Burn
        tx_skeleton.outputs_mut().extend(revert_output.burn_cells);

        let challenger_lock_dep = self.ckb_genesis_info.sighash_dep();
        let challenger_lock = self.wallet.lock_script().to_owned();
        tx_skeleton.cell_deps_mut().push(challenger_lock_dep);
        fill_tx_fee(&mut tx_skeleton, &self.rpc_client, challenger_lock).await?;

        let tx = self.wallet.sign_tx_skeleton(tx_skeleton)?;

        utils::dry_run_transaction(&self.rpc_client, tx.clone(), "revert block").await;
        utils::dump_transaction(
            &self.config.debug_tx_dump_path,
            &self.rpc_client,
            tx.clone(),
        )
        .await;

        let tx_hash = self.rpc_client.send_transaction(tx).await?;
        log::info!("Revert block in tx {}", to_hex(&tx_hash));

        Ok(())
    }

    async fn build_verifier_tx(&self, verifier: (CellOutput, Bytes)) -> Result<Transaction> {
        let mut tx_skeleton = TransactionSkeleton::default();
        tx_skeleton.outputs_mut().push(verifier);

        let challenger_lock_dep = self.ckb_genesis_info.sighash_dep();
        let challenger_lock = self.wallet.lock_script().to_owned();
        tx_skeleton.cell_deps_mut().push(challenger_lock_dep);
        fill_tx_fee(&mut tx_skeleton, &self.rpc_client, challenger_lock).await?;

        self.wallet.sign_tx_skeleton(tx_skeleton)
    }

    async fn build_cancel_tx(
        &self,
        rollup_state: RollupState,
        cancel_output: CancelChallengeOutput,
        challenge_input: InputCellInfo,
        verifier_context: VerifierContext,
        media_time: Duration,
    ) -> Result<Transaction> {
        let mut tx_skeleton = TransactionSkeleton::default();

        // Rollup
        let rollup_deps = vec![
            self.config.rollup_cell_type_dep.clone().into(),
            self.config.rollup_config_cell_dep.clone().into(),
        ];
        let rollup_output = (
            rollup_state.rollup_output(),
            cancel_output.post_global_state.as_bytes(),
        );
        let rollup_witness = cancel_output.rollup_witness;

        tx_skeleton.cell_deps_mut().extend(rollup_deps);
        tx_skeleton.inputs_mut().push(rollup_state.rollup_input());
        tx_skeleton.outputs_mut().push(rollup_output);
        tx_skeleton.witnesses_mut().push(rollup_witness);

        // Challenge
        let challenge_dep = self.config.challenge_cell_lock_dep.clone().into();
        let challenge_witness = cancel_output.challenge_witness;
        tx_skeleton.cell_deps_mut().push(challenge_dep);
        tx_skeleton.inputs_mut().push(challenge_input);
        tx_skeleton.witnesses_mut().push(challenge_witness);

        // Verifier
        let verifier_tx_hash = verifier_context.tx_hash();
        tx_skeleton.cell_deps_mut().push(verifier_context.cell_dep);
        tx_skeleton.inputs_mut().push(verifier_context.input);
        if let Some(verifier_witness) = cancel_output.verifier_witness {
            tx_skeleton.witnesses_mut().push(verifier_witness);
        }

        // Signature verification needs an owner cell
        if !has_lock_cell(&tx_skeleton, &self.wallet.lock_script()) {
            let spent_inputs = verifier_context.spent_inputs;

            let owner_input = self
                .query_owner_cell_for_verifier(verifier_tx_hash, spent_inputs)
                .await?;
            log::debug!("push an owner cell to unlock verifier cell");

            let owner_lock_dep = self.ckb_genesis_info.sighash_dep();
            tx_skeleton.cell_deps_mut().push(owner_lock_dep);
            tx_skeleton.inputs_mut().push(owner_input);
        }

        // Poa
        {
            let poa = self.poa.lock().await;
            poa.fill_poa(&mut tx_skeleton, 0, media_time).await?;
        }

        let owner_lock = self.wallet.lock_script().to_owned();
        fill_tx_fee(&mut tx_skeleton, &self.rpc_client, owner_lock).await?;
        self.wallet.sign_tx_skeleton(tx_skeleton)
    }

    async fn query_owner_cell_for_verifier(
        &self,
        verifier_tx_hash: H256,
        spent_inputs: Option<HashSet<OutPoint>>,
    ) -> Result<InputCellInfo> {
        let rpc_client = &self.rpc_client;
        let owner_lock = self.wallet.lock_script().to_owned();

        if let Ok(Some(cell)) = rpc_client.query_owner_cell(owner_lock, spent_inputs).await {
            return Ok(to_input_cell_info(cell));
        }

        log::debug!("can't find a owner cell for verifier, try wait verifier tx committed");
        self.wait_tx_committed(verifier_tx_hash).await?;

        let owner_lock = self.wallet.lock_script().to_owned();
        let cell = {
            let query = rpc_client.query_owner_cell(owner_lock, None).await?;
            query.ok_or_else(|| anyhow!("can't find an owner cell for verifier"))?
        };

        Ok(to_input_cell_info(cell))
    }

    async fn wait_tx_proposed(&self, tx_hash: H256) -> Result<()> {
        let timeout = Duration::new(30, 0);
        let now = Instant::now();

        loop {
            match self.rpc_client.get_transaction_status(tx_hash).await? {
                Some(TxStatus::Proposed) | Some(TxStatus::Committed) => return Ok(()),
                Some(TxStatus::Pending) => (),
                None => return Err(anyhow!("tx hash {} not found", to_hex(&tx_hash))),
            }

            if now.elapsed() >= timeout {
                return Err(anyhow!("wait tx hash {} timeout", to_hex(&tx_hash)));
            }

            async_std::task::sleep(Duration::new(3, 0)).await;
        }
    }

    async fn wait_tx_committed(&self, tx_hash: H256) -> Result<()> {
        let timeout = Duration::new(30, 0);
        let now = Instant::now();

        loop {
            match self.rpc_client.get_transaction_status(tx_hash).await? {
                Some(TxStatus::Committed) => return Ok(()),
                Some(TxStatus::Proposed) | Some(TxStatus::Pending) => (),
                None => return Err(anyhow!("tx hash {} not found", to_hex(&tx_hash))),
            }

            if now.elapsed() >= timeout {
                return Err(anyhow!("wait tx hash {} timeout", to_hex(&tx_hash)));
            }

            async_std::task::sleep(Duration::new(3, 0)).await;
        }
    }
}

struct RollupState {
    rollup_cell: CellInfo,
    inner: GlobalState,
}

impl RollupState {
    async fn query(rpc_client: &RPCClient) -> Result<Self> {
        let query_cell = rpc_client.query_rollup_cell().await?;

        let rollup_cell = query_cell.ok_or_else(|| anyhow!("rollup cell not found"))?;
        let global_state = GlobalState::from_slice(&rollup_cell.data)?;

        Ok(RollupState {
            rollup_cell,
            inner: global_state,
        })
    }

    fn rollup_input(&self) -> InputCellInfo {
        to_input_cell_info(self.rollup_cell.clone())
    }

    fn rollup_output(&self) -> CellOutput {
        self.rollup_cell.output.clone()
    }

    fn get_state(&self) -> &GlobalState {
        &self.inner
    }

    fn status(&self) -> Result<Status> {
        let status: u8 = self.inner.status().into();
        Status::try_from(status).map_err(|n| anyhow!("invalid status {}", n))
    }
}

#[derive(Clone)]
struct VerifierContext {
    cell_dep: CellDep,
    input: InputCellInfo,
    witness: Option<WitnessArgs>,
    spent_inputs: Option<HashSet<OutPoint>>,
}

impl VerifierContext {
    fn new(
        cell_dep: CellDep,
        input: InputCellInfo,
        witness: Option<WitnessArgs>,
        spent_inputs: Option<HashSet<OutPoint>>,
    ) -> Self {
        VerifierContext {
            cell_dep,
            input,
            witness,
            spent_inputs,
        }
    }

    fn tx_hash(&self) -> H256 {
        self.input.input.previous_output().tx_hash().unpack()
    }
}

fn has_lock_cell(tx_skeleton: &TransactionSkeleton, lock: &Script) -> bool {
    let lock_hash = lock.hash();
    let mut inputs = tx_skeleton.inputs().iter();
    inputs.any(|input| input.cell.output.lock().hash() == lock_hash)
}

fn to_tip_hash(event: &ChainEvent) -> H256 {
    let tip_block = match event {
        ChainEvent::Reverted {
            old_tip: _,
            new_block,
        } => new_block,
        ChainEvent::NewBlock { block } => block,
    };
    tip_block.header().hash().into()
}

fn to_tip_number(event: &ChainEvent) -> u64 {
    let tip_block = match event {
        ChainEvent::Reverted {
            old_tip: _,
            new_block,
        } => new_block,
        ChainEvent::NewBlock { block } => block,
    };
    tip_block.header().raw().number().unpack()
}

fn to_input_cell_info(cell_info: CellInfo) -> InputCellInfo {
    InputCellInfo {
        input: CellInput::new_builder()
            .previous_output(cell_info.out_point.clone())
            .build(),
        cell: cell_info,
    }
}

fn to_input_cell_info_with_since(cell_info: CellInfo, since: u64) -> InputCellInfo {
    InputCellInfo {
        input: CellInput::new_builder()
            .previous_output(cell_info.out_point.clone())
            .since(since.pack())
            .build(),
        cell: cell_info,
    }
}

fn to_hex(hash: &H256) -> String {
    hex::encode(hash.as_slice())
}

fn to_cell_info(cell: ChallengeCell) -> CellInfo {
    CellInfo {
        out_point: cell.input.previous_output(),
        output: cell.output,
        data: cell.output_data,
    }
}

fn extract_inputs(tx: &Transaction) -> HashSet<OutPoint> {
    let inputs = tx.raw().inputs().into_iter();
    inputs.map(|i| i.previous_output()).collect()
}
