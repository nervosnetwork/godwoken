use crate::types::{CellInfo, InputCellInfo};

use anyhow::Result;
use ckb_types::prelude::Entity;
use gw_config::BlockProducerConfig;
use gw_generator::RollupContext;
use gw_types::bytes::Bytes;
use gw_types::core::ScriptHashType;
use gw_types::packed::{
    CellDep, CellInput, CellOutput, CustodianLockArgs, Script, UnlockCustodianViaRevertWitness,
    WitnessArgs,
};
use gw_types::prelude::{Builder, Pack, Unpack};

pub struct RevertedDeposits {
    pub deps: Vec<CellDep>,
    pub inputs: Vec<InputCellInfo>,
    pub witness_args: Vec<WitnessArgs>,
    pub outputs: Vec<(CellOutput, Bytes)>,
}

pub fn revert(
    rollup_context: &RollupContext,
    block_producer_config: &BlockProducerConfig,
    custodian_cells: Vec<CellInfo>,
) -> Result<Option<RevertedDeposits>> {
    if custodian_cells.is_empty() {
        return Ok(None);
    }

    let mut custodian_inputs = vec![];
    let mut custodian_witness = vec![];
    let mut deposit_outputs = vec![];

    let rollup_type_hash = rollup_context.rollup_script_hash.as_slice().iter();
    for revert_custodian in custodian_cells.into_iter() {
        let deposit_lock = {
            let args: Bytes = revert_custodian.output.lock().args().unpack();
            let custodian_lock_args = CustodianLockArgs::from_slice(&args.slice(32..))?;

            let deposit_lock_args = custodian_lock_args.deposit_lock_args();

            let lock_args: Bytes = rollup_type_hash
                .clone()
                .chain(deposit_lock_args.as_slice().iter())
                .cloned()
                .collect();

            Script::new_builder()
                .code_hash(rollup_context.rollup_config.deposit_script_type_hash())
                .hash_type(ScriptHashType::Type.into())
                .args(lock_args.pack())
                .build()
        };

        let deposit_output = {
            let output_builder = revert_custodian.output.clone().as_builder();
            output_builder.lock(deposit_lock.clone()).build()
        };

        let custodian_input = {
            let input = CellInput::new_builder()
                .previous_output(revert_custodian.out_point.clone())
                .build();

            InputCellInfo {
                input,
                cell: revert_custodian.clone(),
            }
        };

        let unlock_custodian_witness = UnlockCustodianViaRevertWitness::new_builder()
            .deposit_lock_hash(deposit_lock.hash().pack())
            .build();

        let revert_custodian_witness_args = WitnessArgs::new_builder()
            .lock(Some(unlock_custodian_witness.as_bytes()).pack())
            .build();

        custodian_inputs.push(custodian_input);
        custodian_witness.push(revert_custodian_witness_args);
        deposit_outputs.push((deposit_output, revert_custodian.data.clone()));
    }

    let custodian_lock_dep = block_producer_config.custodian_cell_lock_dep.clone();
    let sudt_type_dep = block_producer_config.l1_sudt_type_dep.clone();
    let mut cell_deps = vec![custodian_lock_dep.into()];
    if custodian_inputs
        .iter()
        .any(|info| info.cell.output.type_().to_opt().is_some())
    {
        cell_deps.push(sudt_type_dep.into())
    }

    Ok(Some(RevertedDeposits {
        deps: cell_deps,
        inputs: custodian_inputs,
        outputs: deposit_outputs,
        witness_args: custodian_witness,
    }))
}
