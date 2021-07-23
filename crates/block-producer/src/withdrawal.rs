use crate::types::InputCellInfo;
use crate::{
    rpc_client::{CollectedCustodianCells, RPCClient, WithdrawalsAmount},
    types::CellInfo,
};

use anyhow::{anyhow, Result};
use gw_common::CKB_SUDT_SCRIPT_ARGS;
use gw_config::BlockProducerConfig;
use gw_generator::RollupContext;
use gw_types::{
    bytes::Bytes,
    core::ScriptHashType,
    packed::{
        CellDep, CellInput, CellOutput, CustodianLockArgs, DepositLockArgs, GlobalState, L2Block,
        Script, UnlockWithdrawalViaRevert, UnlockWithdrawalWitness, UnlockWithdrawalWitnessUnion,
        WithdrawalLockArgs, WithdrawalRequest, WitnessArgs,
    },
    prelude::*,
};

use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
pub struct AvailableCustodians {
    pub capacity: u128,
    pub sudt: HashMap<[u8; 32], (u128, Script)>,
}

impl Default for AvailableCustodians {
    fn default() -> Self {
        AvailableCustodians {
            capacity: 0,
            sudt: Default::default(),
        }
    }
}

impl<'a> From<&'a CollectedCustodianCells> for AvailableCustodians {
    fn from(collected: &'a CollectedCustodianCells) -> Self {
        AvailableCustodians {
            capacity: collected.capacity,
            sudt: collected.sudt.clone(),
        }
    }
}

#[derive(Clone)]
struct CkbCustodian {
    capacity: u128,
    balance: u128,
    min_capacity: u64,
}

struct SudtCustodian {
    capacity: u64,
    balance: u128,
    script: Script,
}

pub struct Generator<'a> {
    rollup_context: &'a RollupContext,
    ckb_custodian: CkbCustodian,
    sudt_custodians: HashMap<[u8; 32], SudtCustodian>,
    withdrawals: Vec<(CellOutput, Bytes)>,
}

impl<'a> Generator<'a> {
    pub fn new(
        rollup_context: &'a RollupContext,
        available_custodians: AvailableCustodians,
    ) -> Self {
        let mut total_sudt_capacity = 0u128;
        let mut sudt_custodians = HashMap::new();

        for (sudt_type_hash, (balance, type_script)) in available_custodians.sudt.into_iter() {
            let (change, _data) =
                generate_finalized_custodian(rollup_context, balance, type_script.clone());

            let sudt_custodian = SudtCustodian {
                capacity: change.capacity().unpack(),
                balance,
                script: type_script,
            };

            total_sudt_capacity =
                total_sudt_capacity.saturating_add(sudt_custodian.capacity as u128);
            sudt_custodians.insert(sudt_type_hash, sudt_custodian);
        }

        let ckb_custodian_min_capacity = {
            let lock = build_finalized_custodian_lock(rollup_context);
            (8 + lock.as_slice().len() as u64) * 100000000
        };
        let ckb_custodian_capacity = available_custodians
            .capacity
            .saturating_sub(total_sudt_capacity);
        let ckb_balance = ckb_custodian_capacity.saturating_sub(ckb_custodian_min_capacity as u128);

        let ckb_custodian = CkbCustodian {
            capacity: ckb_custodian_capacity,
            balance: ckb_balance,
            min_capacity: ckb_custodian_min_capacity,
        };

        Generator {
            rollup_context,
            ckb_custodian,
            sudt_custodians,
            withdrawals: Default::default(),
        }
    }

    pub fn verified_output(
        &self,
        req: &WithdrawalRequest,
        block: &L2Block,
    ) -> Result<(CellOutput, Bytes)> {
        // Verify finalized custodian exists
        let req_sudt: u128 = req.raw().amount().unpack();
        let sudt_type_hash: [u8; 32] = req.raw().sudt_script_hash().unpack();
        if 0 != req_sudt && !self.sudt_custodians.contains_key(&sudt_type_hash) {
            return Err(anyhow!("no finalized sudt custodian for {}", req));
        }

        // Verify minimal capacity
        let sudt_script = {
            let sudt_custodian = self.sudt_custodians.get(&sudt_type_hash);
            sudt_custodian.map(|sudt| sudt.script.to_owned())
        };
        let output = generate_withdrawal_output(req, self.rollup_context, block, sudt_script)
            .map_err(|min_capacity| anyhow!("{} minimal capacity for {}", min_capacity, req))?;

        // Verify remaind sudt
        let mut ckb_custodian = self.ckb_custodian.clone();
        if 0 != req_sudt {
            let sudt_custodian = match self.sudt_custodians.get(&sudt_type_hash) {
                Some(custodian) => custodian,
                None => return Err(anyhow!("no finalized sudt custodian for {}", req)),
            };

            let remaind = sudt_custodian
                .balance
                .checked_sub(req_sudt)
                .ok_or_else(|| anyhow!("no enough custodian sudt for {}", req))?;

            // Consume all remaind sudt, give sudt custodian capacity back to ckb custodian
            if 0 == remaind {
                // If ckb custodian is already consumed
                if 0 == ckb_custodian.capacity {
                    ckb_custodian.capacity = sudt_custodian.capacity as u128;
                    ckb_custodian.balance =
                        (sudt_custodian.capacity - ckb_custodian.min_capacity) as u128;
                } else {
                    ckb_custodian.capacity += sudt_custodian.capacity as u128;
                    ckb_custodian.balance += sudt_custodian.capacity as u128;
                }
            }
        }

        // Verify remaind ckb
        let req_ckb = req.raw().capacity().unpack() as u128;
        match ckb_custodian.balance.checked_sub(req_ckb) {
            Some(_) => Ok(output),
            // Consume all remaind ckb
            None if req_ckb == ckb_custodian.capacity => Ok(output),
            // No able to cover withdrawal cell and ckb custodian change
            None => Err(anyhow!("no enough custodian capacity(*change) for {}", req)),
        }
    }

    pub fn include_and_verify(&mut self, req: &WithdrawalRequest, block: &L2Block) -> Result<()> {
        let verified_output = self.verified_output(req, block)?;
        let ckb_custodian = &mut self.ckb_custodian;

        // Update custodians according to verified output
        let req_sudt: u128 = req.raw().amount().unpack();
        if 0 != req_sudt {
            let sudt_type_hash: [u8; 32] = req.raw().sudt_script_hash().unpack();
            let sudt_custodian = match self.sudt_custodians.get_mut(&sudt_type_hash) {
                Some(custodian) => custodian,
                None => return Err(anyhow!("unexpected sudt not found for verified {}", req)),
            };

            match sudt_custodian.balance.checked_sub(req_sudt) {
                Some(remaind) => sudt_custodian.balance = remaind,
                None => return Err(anyhow!("unexpected sudt overflow for verified {}", req)),
            }

            // Consume all remaind sudt, give sudt custodian capacity back to ckb custodian
            if 0 == sudt_custodian.balance {
                if 0 == ckb_custodian.capacity {
                    ckb_custodian.capacity = sudt_custodian.capacity as u128;
                    ckb_custodian.balance =
                        (sudt_custodian.capacity - ckb_custodian.min_capacity) as u128;
                } else {
                    ckb_custodian.capacity += sudt_custodian.capacity as u128;
                    ckb_custodian.balance += sudt_custodian.capacity as u128;
                }
                sudt_custodian.capacity = 0;
            }
        }

        let req_ckb = req.raw().capacity().unpack() as u128;
        match ckb_custodian.balance.checked_sub(req_ckb) {
            Some(remaind) => {
                ckb_custodian.capacity -= req_ckb;
                ckb_custodian.balance = remaind;
            }
            // Consume all remaind ckb
            None if req_ckb == ckb_custodian.capacity => {
                ckb_custodian.capacity = 0;
                ckb_custodian.balance = 0;
            }
            None => return Err(anyhow!("unexpected capacity overflow for verified {}", req)),
        }

        self.withdrawals.push(verified_output);
        Ok(())
    }

    pub fn finish(self) -> Vec<(CellOutput, Bytes)> {
        let mut outputs = self.withdrawals;
        let custodian_lock = build_finalized_custodian_lock(self.rollup_context);

        // Generate sudt custodian changes
        let sudt_changes = {
            let custodians = self.sudt_custodians.into_iter();
            custodians.filter(|(_, custodian)| 0 != custodian.capacity && 0 != custodian.balance)
        };
        for custodian in sudt_changes.map(|(_, c)| c) {
            let output = CellOutput::new_builder()
                .capacity(custodian.capacity.pack())
                .type_(Some(custodian.script).pack())
                .lock(custodian_lock.clone())
                .build();

            outputs.push((output, custodian.balance.pack().as_bytes()));
        }

        // Generate ckb custodian change
        let build_ckb_output = |capacity: u64| -> (CellOutput, Bytes) {
            let output = CellOutput::new_builder()
                .capacity(capacity.pack())
                .lock(custodian_lock.clone())
                .build();
            (output, Bytes::new())
        };
        if 0 != self.ckb_custodian.capacity {
            if self.ckb_custodian.capacity < u64::MAX as u128 {
                outputs.push(build_ckb_output(self.ckb_custodian.capacity as u64));
                return outputs;
            }

            let ckb_custodian = self.ckb_custodian;
            let mut remaind = ckb_custodian.capacity;
            while remaind > 0 {
                let max = remaind.saturating_sub(ckb_custodian.min_capacity as u128);
                match max.checked_sub(u64::MAX as u128) {
                    Some(cap) => {
                        outputs.push(build_ckb_output(u64::MAX));
                        remaind = cap.saturating_add(ckb_custodian.min_capacity as u128);
                    }
                    None if max.saturating_add(ckb_custodian.min_capacity as u128)
                        > u64::MAX as u128 =>
                    {
                        let max = max.saturating_add(ckb_custodian.min_capacity as u128);
                        let half = max / 2;
                        outputs.push(build_ckb_output(half as u64));
                        outputs.push(build_ckb_output(max.saturating_sub(half) as u64));
                        remaind = 0;
                    }
                    None => {
                        outputs.push(build_ckb_output(
                            (max as u64).saturating_add(ckb_custodian.min_capacity),
                        ));
                        remaind = 0;
                    }
                }
            }
        }

        outputs
    }
}

pub struct GeneratedWithdrawals {
    pub deps: Vec<CellDep>,
    pub inputs: Vec<InputCellInfo>,
    pub outputs: Vec<(CellOutput, Bytes)>,
}

// Note: custodian lock search rollup cell in inputs
pub async fn generate(
    input_rollup_cell: &CellInfo,
    rollup_context: &RollupContext,
    block: &L2Block,
    block_producer_config: &BlockProducerConfig,
    rpc_client: &RPCClient,
) -> Result<Option<GeneratedWithdrawals>> {
    if block.withdrawals().is_empty() {
        return Ok(None);
    }

    let global_state = GlobalState::from_slice(&input_rollup_cell.data)
        .map_err(|_| anyhow!("parse rollup cell global state"))?;
    let last_finalized_block_number = global_state.last_finalized_block_number().unpack();

    let total_withdrawal_amount = sum(block.withdrawals().into_iter());
    let custodian_cells = rpc_client
        .query_finalized_custodian_cells(&total_withdrawal_amount, last_finalized_block_number)
        .await?;
    log::debug!("custodian inputs {:?}", custodian_cells);

    let mut generator = Generator::new(rollup_context, (&custodian_cells).into());
    for req in block.withdrawals().into_iter() {
        generator
            .include_and_verify(&req, block)
            .map_err(|err| anyhow!("unexpected withdrawal err {}", err))?
    }
    log::debug!("included withdrawals {}", generator.withdrawals.len());

    let custodian_lock_dep = block_producer_config.custodian_cell_lock_dep.clone();
    let sudt_type_dep = block_producer_config.l1_sudt_type_dep.clone();
    let mut cell_deps = vec![custodian_lock_dep.into()];
    if !total_withdrawal_amount.sudt.is_empty() {
        cell_deps.push(sudt_type_dep.into());
    }

    let custodian_inputs = custodian_cells.cells_info.into_iter().map(|cell| {
        let input = CellInput::new_builder()
            .previous_output(cell.out_point.clone())
            .build();
        InputCellInfo { input, cell }
    });

    let generated_withdrawals = GeneratedWithdrawals {
        deps: cell_deps,
        inputs: custodian_inputs.collect(),
        outputs: generator.finish(),
    };

    Ok(Some(generated_withdrawals))
}

pub struct RevertedWithdrawals {
    pub deps: Vec<CellDep>,
    pub inputs: Vec<InputCellInfo>,
    pub witness_args: Vec<WitnessArgs>,
    pub outputs: Vec<(CellOutput, Bytes)>,
}

pub fn revert(
    rollup_context: &RollupContext,
    block_producer_config: &BlockProducerConfig,
    withdrawal_cells: Vec<CellInfo>,
) -> Result<Option<RevertedWithdrawals>> {
    if withdrawal_cells.is_empty() {
        return Ok(None);
    }

    let mut withdrawal_inputs = vec![];
    let mut withdrawal_witness = vec![];
    let mut custodian_outputs = vec![];

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("unexpected timestamp")
        .as_millis() as u64;

    // We use timestamp plus idx and rollup_type_hash to create different custodian lock
    // hash for every reverted withdrawal input. Withdrawal lock use custodian lock hash to
    // index corresponding custodian output.
    // NOTE: These locks must also be different from custodian change cells created by
    // withdrawal requests processing.
    let rollup_type_hash = rollup_context.rollup_script_hash.as_slice().iter();
    for (idx, withdrawal) in withdrawal_cells.into_iter().enumerate() {
        let custodian_lock = {
            let deposit_lock_args = DepositLockArgs::new_builder()
                .owner_lock_hash(rollup_context.rollup_script_hash.pack())
                .cancel_timeout((idx as u64 + timestamp).pack())
                .build();

            let custodian_lock_args = CustodianLockArgs::new_builder()
                .deposit_lock_args(deposit_lock_args)
                .build();

            let lock_args: Bytes = rollup_type_hash
                .clone()
                .chain(custodian_lock_args.as_slice().iter())
                .cloned()
                .collect();

            Script::new_builder()
                .code_hash(rollup_context.rollup_config.custodian_script_type_hash())
                .hash_type(ScriptHashType::Type.into())
                .args(lock_args.pack())
                .build()
        };

        let custodian_output = {
            let output_builder = withdrawal.output.clone().as_builder();
            output_builder.lock(custodian_lock.clone()).build()
        };

        let withdrawal_input = {
            let input = CellInput::new_builder()
                .previous_output(withdrawal.out_point.clone())
                .build();

            InputCellInfo {
                input,
                cell: withdrawal.clone(),
            }
        };

        let unlock_withdrawal_witness = {
            let unlock_withdrawal_via_revert = UnlockWithdrawalViaRevert::new_builder()
                .custodian_lock_hash(custodian_lock.hash().pack())
                .build();

            UnlockWithdrawalWitness::new_builder()
                .set(UnlockWithdrawalWitnessUnion::UnlockWithdrawalViaRevert(
                    unlock_withdrawal_via_revert,
                ))
                .build()
        };
        let withdrawal_witness_args = WitnessArgs::new_builder()
            .lock(Some(unlock_withdrawal_witness.as_bytes()).pack())
            .build();

        withdrawal_inputs.push(withdrawal_input);
        withdrawal_witness.push(withdrawal_witness_args);
        custodian_outputs.push((custodian_output, withdrawal.data.clone()));
    }

    let withdrawal_lock_dep = block_producer_config.withdrawal_cell_lock_dep.clone();
    let sudt_type_dep = block_producer_config.l1_sudt_type_dep.clone();
    let mut cell_deps = vec![withdrawal_lock_dep.into()];
    if withdrawal_inputs
        .iter()
        .any(|info| info.cell.output.type_().to_opt().is_some())
    {
        cell_deps.push(sudt_type_dep.into())
    }

    Ok(Some(RevertedWithdrawals {
        deps: cell_deps,
        inputs: withdrawal_inputs,
        outputs: custodian_outputs,
        witness_args: withdrawal_witness,
    }))
}

fn sum<Iter: Iterator<Item = WithdrawalRequest>>(reqs: Iter) -> WithdrawalsAmount {
    reqs.fold(
        WithdrawalsAmount::default(),
        |mut total_amount, withdrawal| {
            total_amount.capacity = total_amount
                .capacity
                .saturating_add(withdrawal.raw().capacity().unpack() as u128);

            let sudt_script_hash = withdrawal.raw().sudt_script_hash().unpack();
            let sudt_amount = withdrawal.raw().amount().unpack();
            if sudt_amount != 0 {
                match sudt_script_hash {
                    CKB_SUDT_SCRIPT_ARGS => {
                        let account = withdrawal.raw().account_script_hash();
                        log::warn!("{} withdrawal request non-zero sudt amount but it's type hash ckb, ignore this amount", account);
                    }
                    _ => {
                        let total_sudt_amount = total_amount.sudt.entry(sudt_script_hash).or_insert(0u128);
                        *total_sudt_amount = total_sudt_amount.saturating_add(sudt_amount);
                    }
                }
            }

            total_amount
        }
    )
}

fn build_withdrawal_lock(
    req: &WithdrawalRequest,
    rollup_context: &RollupContext,
    block: &L2Block,
) -> Script {
    let withdrawal_capacity: u64 = req.raw().capacity().unpack();
    let lock_args: Bytes = {
        let withdrawal_lock_args = WithdrawalLockArgs::new_builder()
            .account_script_hash(req.raw().account_script_hash())
            .withdrawal_block_hash(block.hash().pack())
            .withdrawal_block_number(block.raw().number())
            .sudt_script_hash(req.raw().sudt_script_hash())
            .sell_amount(req.raw().sell_amount())
            .sell_capacity(withdrawal_capacity.pack())
            .owner_lock_hash(req.raw().owner_lock_hash())
            .payment_lock_hash(req.raw().payment_lock_hash())
            .build();

        let rollup_type_hash = rollup_context.rollup_script_hash.as_slice().iter();
        rollup_type_hash
            .chain(withdrawal_lock_args.as_slice().iter())
            .cloned()
            .collect()
    };

    Script::new_builder()
        .code_hash(rollup_context.rollup_config.withdrawal_script_type_hash())
        .hash_type(ScriptHashType::Type.into())
        .args(lock_args.pack())
        .build()
}

fn build_finalized_custodian_lock(rollup_context: &RollupContext) -> Script {
    let rollup_type_hash = rollup_context.rollup_script_hash.as_slice().iter();
    let custodian_lock_args = CustodianLockArgs::default();

    let args: Bytes = rollup_type_hash
        .chain(custodian_lock_args.as_slice().iter())
        .cloned()
        .collect();

    Script::new_builder()
        .code_hash(rollup_context.rollup_config.custodian_script_type_hash())
        .hash_type(ScriptHashType::Type.into())
        .args(args.pack())
        .build()
}

fn generate_withdrawal_output(
    req: &WithdrawalRequest,
    rollup_context: &RollupContext,
    block: &L2Block,
    type_script: Option<Script>,
) -> std::result::Result<(CellOutput, Bytes), u64> {
    let req_ckb: u64 = req.raw().capacity().unpack();
    let lock = build_withdrawal_lock(req, rollup_context, block);
    let (type_, data) = match type_script {
        Some(type_) => (Some(type_).pack(), req.raw().amount().as_bytes()),
        None => (None::<Script>.pack(), Bytes::new()),
    };

    let size = 8 + data.len() + type_.as_slice().len() + lock.as_slice().len();
    let min_capacity = size as u64 * 100_000_000;

    if req_ckb < min_capacity {
        return Err(min_capacity);
    }

    let withdrawal = CellOutput::new_builder()
        .capacity(req_ckb.pack())
        .lock(lock)
        .type_(type_)
        .build();

    Ok((withdrawal, data))
}

fn generate_finalized_custodian(
    rollup_context: &RollupContext,
    amount: u128,
    type_: Script,
) -> (CellOutput, Bytes) {
    let lock = build_finalized_custodian_lock(rollup_context);
    let data = amount.pack();

    let capacity = {
        let size = 8 + data.as_slice().len() + type_.as_slice().len() + lock.as_slice().len();
        size as u64 * 100000000u64
    };

    let output = CellOutput::new_builder()
        .capacity(capacity.pack())
        .type_(Some(type_).pack())
        .lock(lock)
        .build();

    (output, data.as_bytes())
}
