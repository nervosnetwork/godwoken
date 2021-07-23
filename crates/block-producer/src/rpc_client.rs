#![allow(clippy::clippy::mutable_key_type)]

use crate::indexer_types::{Cell, Order, Pagination, ScriptType, SearchKey, SearchKeyFilter};
use crate::types::{CellInfo, TxStatus};
use anyhow::{anyhow, Result};
use async_jsonrpc_client::{HttpClient, Output, Params as ClientParams, Transport};
use ckb_types::prelude::Entity;
use gw_common::{CKB_SUDT_SCRIPT_ARGS, H256};
use gw_generator::RollupContext;
use gw_jsonrpc_types::ckb_jsonrpc_types::{self, BlockNumber, Uint32};
use gw_types::{
    bytes::Bytes,
    core::ScriptHashType,
    packed::{
        Block, CellOutput, CustodianLockArgs, CustodianLockArgsReader, DepositLockArgs,
        DepositLockArgsReader, DepositRequest, NumberHash, OutPoint, Script, StakeLockArgs,
        StakeLockArgsReader, Transaction, WithdrawalLockArgs, WithdrawalLockArgsReader,
    },
    prelude::*,
};
use serde::de::DeserializeOwned;
use serde_json::{from_value, json};

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

const DEFAULT_QUERY_LIMIT: usize = 1000;

lazy_static::lazy_static! {
    /// CKB built-in type ID code hash
    static ref TYPE_ID_CODE_HASH: [u8; 32] = {
        let hexed_type_id_code_hash: &str = "00000000000000000000000000000000000000000000000000545950455f4944";
        let mut code_hash = [0u8; 32];
        faster_hex::hex_decode(hexed_type_id_code_hash.as_bytes(), &mut code_hash).expect("dehex type id code_hash");
        code_hash
    };
}

#[derive(Debug, Clone)]
pub struct DepositInfo {
    pub request: DepositRequest,
    pub cell: CellInfo,
}

type JsonH256 = ckb_fixed_hash::H256;

fn to_h256(v: JsonH256) -> H256 {
    let h: [u8; 32] = v.into();
    h.into()
}

fn to_jsonh256(v: H256) -> JsonH256 {
    let h: [u8; 32] = v.into();
    h.into()
}

fn to_result<T: DeserializeOwned>(output: Output) -> anyhow::Result<T> {
    match output {
        Output::Success(success) => Ok(from_value(success.result)?),
        Output::Failure(failure) => Err(anyhow::anyhow!("JSONRPC error: {}", failure.error)),
    }
}

fn to_cell_info(cell: Cell) -> CellInfo {
    let out_point = {
        let out_point: ckb_types::packed::OutPoint = cell.out_point.into();
        OutPoint::new_unchecked(out_point.as_bytes())
    };
    let output = {
        let output: ckb_types::packed::CellOutput = cell.output.into();
        CellOutput::new_unchecked(output.as_bytes())
    };
    let data = cell.output_data.into_bytes();

    CellInfo {
        out_point,
        output,
        data,
    }
}

fn parse_deposit_request(
    output: &CellOutput,
    output_data: &Bytes,
    deposit_lock_args: &DepositLockArgs,
) -> Option<DepositRequest> {
    let capacity = output.capacity();
    let script = deposit_lock_args.layer2_lock();
    let (sudt_script_hash, amount) = {
        match output.type_().to_opt() {
            Some(type_) => {
                let mut buf = [0u8; 16];
                if output_data.len() < buf.len() {
                    return None;
                }
                let len = buf.len();
                buf.copy_from_slice(&output_data[..len]);
                (type_.hash(), u128::from_le_bytes(buf))
            }
            None => ([0u8; 32], 0),
        }
    };

    let request = DepositRequest::new_builder()
        .script(script)
        .capacity(capacity)
        .amount(amount.pack())
        .sudt_script_hash(sudt_script_hash.pack())
        .build();
    Some(request)
}

#[derive(Debug)]
pub struct WithdrawalsAmount {
    pub capacity: u128,
    pub sudt: HashMap<[u8; 32], u128>,
}

impl Default for WithdrawalsAmount {
    fn default() -> Self {
        WithdrawalsAmount {
            capacity: 0,
            sudt: Default::default(),
        }
    }
}

#[derive(Debug)]
pub struct CollectedCustodianCells {
    pub cells_info: Vec<CellInfo>,
    pub capacity: u128,
    pub sudt: HashMap<[u8; 32], (u128, Script)>,
}

impl Default for CollectedCustodianCells {
    fn default() -> Self {
        CollectedCustodianCells {
            cells_info: Default::default(),
            capacity: 0,
            sudt: Default::default(),
        }
    }
}

#[derive(Clone)]
pub struct RPCClient {
    pub indexer_client: HttpClient,
    pub ckb_client: HttpClient,
    pub rollup_type_script: ckb_types::packed::Script,
    pub rollup_context: RollupContext,
}

impl RPCClient {
    /// query lived rollup cell
    pub async fn query_rollup_cell(&self) -> Result<Option<CellInfo>> {
        let search_key = SearchKey {
            script: self.rollup_type_script.clone().into(),
            script_type: ScriptType::Type,
            filter: Some(SearchKeyFilter {
                script: None,
                output_data_len_range: None,
                output_capacity_range: None,
                block_range: None,
            }),
        };
        let order = Order::Desc;
        let limit = Uint32::from(1);

        let mut cells: Pagination<Cell> = to_result(
            self.indexer_client
                .request(
                    "get_cells",
                    Some(ClientParams::Array(vec![
                        json!(search_key),
                        json!(order),
                        json!(limit),
                    ])),
                )
                .await?,
        )?;
        if let Some(cell) = cells.objects.pop() {
            let out_point = {
                let out_point: ckb_types::packed::OutPoint = cell.out_point.into();
                OutPoint::new_unchecked(out_point.as_bytes())
            };
            let output = {
                let output: ckb_types::packed::CellOutput = cell.output.into();
                CellOutput::new_unchecked(output.as_bytes())
            };
            let data = cell.output_data.into_bytes();
            let cell_info = CellInfo {
                out_point,
                output,
                data,
            };
            return Ok(Some(cell_info));
        }
        Ok(None)
    }

    /// this function queries identity cell by args
    pub async fn query_identity_cell(&self, args: Bytes) -> Result<Option<CellInfo>> {
        let search_key = SearchKey {
            script: ckb_types::packed::Script::new_builder()
                .code_hash(ckb_types::prelude::Pack::pack(&*TYPE_ID_CODE_HASH))
                .hash_type(ScriptHashType::Type.into())
                .args(ckb_types::prelude::Pack::pack(&args))
                .build()
                .into(),
            script_type: ScriptType::Type,
            filter: None,
        };
        let order = Order::Desc;
        let limit = Uint32::from(DEFAULT_QUERY_LIMIT as u32);

        let mut cell = None;
        let mut cursor = None;
        while cell.is_none() {
            let cells: Pagination<Cell> = to_result(
                self.indexer_client
                    .request(
                        "get_cells",
                        Some(ClientParams::Array(vec![
                            json!(search_key),
                            json!(order),
                            json!(limit),
                            json!(cursor),
                        ])),
                    )
                    .await?,
            )?;
            cursor = Some(cells.last_cursor);
            assert!(
                cells.objects.len() <= 1,
                "Never returns more than 1 identity cells"
            );
            cell = cells.objects.into_iter().find_map(|cell| {
                let out_point = {
                    let out_point: ckb_types::packed::OutPoint = cell.out_point.into();
                    OutPoint::new_unchecked(out_point.as_bytes())
                };
                let output = {
                    let output: ckb_types::packed::CellOutput = cell.output.into();
                    CellOutput::new_unchecked(output.as_bytes())
                };
                let data = cell.output_data.into_bytes();
                Some(CellInfo {
                    out_point,
                    output,
                    data,
                })
            });
        }
        Ok(cell)
    }

    /// this function return a cell that do not has data & _type fields
    pub async fn query_owner_cell(
        &self,
        lock: Script,
        filter_inputs: Option<HashSet<OutPoint>>,
    ) -> Result<Option<CellInfo>> {
        let search_key = SearchKey {
            script: {
                let lock = ckb_types::packed::Script::new_unchecked(lock.as_bytes());
                lock.into()
            },
            script_type: ScriptType::Lock,
            filter: None,
        };
        let order = Order::Desc;
        let limit = Uint32::from(DEFAULT_QUERY_LIMIT as u32);

        let mut cell = None;
        let mut cursor = None;
        while cell.is_none() {
            let cells: Pagination<Cell> = to_result(
                self.indexer_client
                    .request(
                        "get_cells",
                        Some(ClientParams::Array(vec![
                            json!(search_key),
                            json!(order),
                            json!(limit),
                            json!(cursor),
                        ])),
                    )
                    .await?,
            )?;

            if cells.last_cursor.is_empty() {
                return Err(anyhow!("no owner cell"));
            }
            cursor = Some(cells.last_cursor);

            cell = cells.objects.into_iter().find_map(|cell| {
                // delete cells with data & type
                if !cell.output_data.is_empty() || cell.output.type_.is_some() {
                    return None;
                }
                let out_point = {
                    let out_point: ckb_types::packed::OutPoint = cell.out_point.clone().into();
                    OutPoint::new_unchecked(out_point.as_bytes())
                };
                match filter_inputs {
                    Some(ref filter_inputs) if filter_inputs.contains(&out_point) => None,
                    _ => Some(to_cell_info(cell)),
                }
            });
        }
        Ok(cell)
    }

    /// query payment cells, the returned cells should provide at least required_capacity fee,
    /// and the remained fees should be enough to cover a charge cell
    pub async fn query_payment_cells(
        &self,
        lock: Script,
        required_capacity: u64,
        taken_outpoints: &HashSet<OutPoint>,
    ) -> Result<Vec<CellInfo>> {
        let search_key = SearchKey {
            script: {
                let lock = ckb_types::packed::Script::new_unchecked(lock.as_bytes());
                lock.into()
            },
            script_type: ScriptType::Lock,
            filter: None,
        };
        let order = Order::Desc;
        let limit = Uint32::from(DEFAULT_QUERY_LIMIT as u32);

        let mut collected_cells = Vec::new();
        let mut collected_capacity = 0u64;
        let mut cursor = None;
        while collected_capacity < required_capacity {
            let cells: Pagination<Cell> = to_result(
                self.indexer_client
                    .request(
                        "get_cells",
                        Some(ClientParams::Array(vec![
                            json!(search_key),
                            json!(order),
                            json!(limit),
                            json!(cursor),
                        ])),
                    )
                    .await?,
            )?;

            if cells.last_cursor.is_empty() {
                return Err(anyhow!("no enough payment cells"));
            }
            cursor = Some(cells.last_cursor);

            let cells = cells.objects.into_iter().filter_map(|cell| {
                let out_point = {
                    let out_point: ckb_types::packed::OutPoint = cell.out_point.into();
                    OutPoint::new_unchecked(out_point.as_bytes())
                };
                // delete cells with data & type
                if !cell.output_data.is_empty()
                    || cell.output.type_.is_some()
                    || taken_outpoints.contains(&out_point)
                {
                    return None;
                }
                let output = {
                    let output: ckb_types::packed::CellOutput = cell.output.into();
                    CellOutput::new_unchecked(output.as_bytes())
                };
                let data = cell.output_data.into_bytes();
                Some(CellInfo {
                    out_point,
                    output,
                    data,
                })
            });

            // collect least cells
            for cell in cells {
                collected_capacity =
                    collected_capacity.saturating_add(cell.output.capacity().unpack());
                collected_cells.push(cell);
                if collected_capacity >= required_capacity {
                    break;
                }
            }
        }
        Ok(collected_cells)
    }

    pub async fn get_cell(&self, out_point: OutPoint) -> Result<Option<CellInfo>> {
        let json_out_point: ckb_jsonrpc_types::OutPoint = {
            let out_point = ckb_types::packed::OutPoint::new_unchecked(out_point.as_bytes());
            out_point.into()
        };
        let cell_with_status: Option<ckb_jsonrpc_types::CellWithStatus> = to_result(
            self.ckb_client
                .request(
                    "get_live_cell",
                    Some(ClientParams::Array(vec![
                        json!(json_out_point),
                        json!(true),
                    ])),
                )
                .await?,
        )?;
        let cell_info = if let Some(cell_with_status) = cell_with_status {
            cell_with_status.cell.map(|cell| {
                let output: ckb_types::packed::CellOutput = cell.output.into();
                let output = CellOutput::new_unchecked(output.as_bytes());
                let data = cell
                    .data
                    .map(|cell_data| cell_data.content.into_bytes())
                    .unwrap_or_else(Bytes::new);
                let out_point = out_point.to_owned();
                CellInfo {
                    output,
                    data,
                    out_point,
                }
            })
        } else {
            None
        };
        Ok(cell_info)
    }

    pub async fn get_tip(&self) -> Result<NumberHash> {
        let number_hash: gw_jsonrpc_types::blockchain::NumberHash =
            to_result(self.indexer_client.request("get_tip", None).await?)?;
        Ok(number_hash.into())
    }

    pub async fn get_block_median_time(&self, block_hash: H256) -> Result<Duration> {
        let median_time: gw_jsonrpc_types::ckb_jsonrpc_types::Uint64 = to_result(
            self.ckb_client
                .request(
                    "get_block_median_time",
                    Some(ClientParams::Array(vec![json!(to_jsonh256(block_hash))])),
                )
                .await?,
        )?;
        Ok(Duration::from_millis(median_time.into()))
    }

    pub async fn get_block_by_number(&self, number: u64) -> Result<Option<Block>> {
        let block_number = BlockNumber::from(number);
        let block_opt: Option<ckb_jsonrpc_types::BlockView> = to_result(
            self.ckb_client
                .request(
                    "get_block_by_number",
                    Some(ClientParams::Array(vec![json!(block_number)])),
                )
                .await?,
        )?;
        Ok(block_opt.map(|b| {
            let block: ckb_types::core::BlockView = b.into();
            Block::new_unchecked(block.data().as_bytes())
        }))
    }

    /// return all lived deposit requests
    /// NOTICE the returned cells may contains invalid cells.
    pub async fn query_deposit_cells(&self) -> Result<Vec<DepositInfo>> {
        const BLOCKS_TO_SEARCH: u64 = 100;
        const LIMIT: u32 = 100;

        let tip_number = self.get_tip().await?.number().unpack();
        let mut deposit_infos = Vec::new();

        let rollup_type_hash: Bytes = self
            .rollup_context
            .rollup_script_hash
            .as_slice()
            .to_vec()
            .into();

        let script = Script::new_builder()
            .args(rollup_type_hash.pack())
            .code_hash(self.rollup_context.rollup_config.deposit_script_type_hash())
            .hash_type(ScriptHashType::Type.into())
            .build();

        let script = {
            let lock = ckb_types::packed::Script::new_unchecked(script.as_bytes());
            lock.into()
        };

        let search_key = SearchKey {
            script,
            script_type: ScriptType::Lock,
            filter: Some(SearchKeyFilter {
                script: None,
                output_data_len_range: None,
                output_capacity_range: None,
                block_range: Some([
                    BlockNumber::from(tip_number.saturating_sub(BLOCKS_TO_SEARCH)),
                    BlockNumber::from(u64::max_value()),
                ]),
            }),
        };
        let order = Order::Asc;
        let limit = Uint32::from(LIMIT);

        let cells: Pagination<Cell> = to_result(
            self.indexer_client
                .request(
                    "get_cells",
                    Some(ClientParams::Array(vec![
                        json!(search_key),
                        json!(order),
                        json!(limit),
                    ])),
                )
                .await?,
        )?;

        let cells = cells.objects.into_iter().map(|cell| {
            let out_point = {
                let out_point: ckb_types::packed::OutPoint = cell.out_point.into();
                OutPoint::new_unchecked(out_point.as_bytes())
            };
            let output = {
                let output: ckb_types::packed::CellOutput = cell.output.into();
                CellOutput::new_unchecked(output.as_bytes())
            };
            let data = cell.output_data.into_bytes();
            CellInfo {
                out_point,
                output,
                data,
            }
        });

        for cell in cells {
            let args: Bytes = cell.output.lock().args().unpack();
            let deposit_lock_args = match DepositLockArgsReader::verify(&args[32..], false) {
                Ok(()) => DepositLockArgs::new_unchecked(args.slice(32..)),
                Err(_) => {
                    log::debug!("invalid deposit cell args: \n{:#x}", args);
                    continue;
                }
            };
            let request = match parse_deposit_request(&cell.output, &cell.data, &deposit_lock_args)
            {
                Some(r) => r,
                None => {
                    log::debug!("invalid deposit cell: \n{:?}", cell);
                    continue;
                }
            };

            let info = DepositInfo { cell, request };
            deposit_infos.push(info);
        }

        Ok(deposit_infos)
    }

    /// query stake
    /// return cell which stake_block_number is less than last_finalized_block_number if the args isn't none
    /// otherwise return stake cell randomly
    pub async fn query_stake(
        &self,
        rollup_context: &RollupContext,
        owner_lock_hash: [u8; 32],
        last_finalized_block_number: Option<u64>,
    ) -> Result<Option<CellInfo>> {
        let lock = Script::new_builder()
            .code_hash(rollup_context.rollup_config.stake_script_type_hash())
            .hash_type(ScriptHashType::Type.into())
            .args(rollup_context.rollup_script_hash.as_slice().pack())
            .build();

        let search_key = SearchKey {
            script: {
                let lock = ckb_types::packed::Script::new_unchecked(lock.as_bytes());
                lock.into()
            },
            script_type: ScriptType::Lock,
            filter: Some(SearchKeyFilter {
                script: None,
                output_data_len_range: None,
                output_capacity_range: None,
                block_range: None,
            }),
        };
        let order = Order::Desc;
        let limit = Uint32::from(DEFAULT_QUERY_LIMIT as u32);

        let mut stake_cell = None;
        let mut cursor = None;

        while stake_cell.is_none() {
            let cells: Pagination<Cell> = to_result(
                self.indexer_client
                    .request(
                        "get_cells",
                        Some(ClientParams::Array(vec![
                            json!(search_key),
                            json!(order),
                            json!(limit),
                            json!(cursor),
                        ])),
                    )
                    .await?,
            )?;

            if cells.last_cursor.is_empty() {
                log::debug!("no unlocked stake");
                return Ok(None);
            }
            cursor = Some(cells.last_cursor);

            stake_cell = cells.objects.into_iter().find(|cell| {
                let args = cell.output.lock.args.clone().into_bytes();
                let stake_lock_args = match StakeLockArgsReader::verify(&args[32..], false) {
                    Ok(()) => StakeLockArgs::new_unchecked(args.slice(32..)),
                    Err(_) => return false,
                };
                match last_finalized_block_number {
                    Some(last_finalized_block_number) => {
                        stake_lock_args.stake_block_number().unpack() <= last_finalized_block_number
                            && stake_lock_args.owner_lock_hash().as_slice() == owner_lock_hash
                    }
                    None => stake_lock_args.owner_lock_hash().as_slice() == owner_lock_hash,
                }
            });
        }

        let fetch_cell_info = |cell: Cell| {
            let out_point = {
                let out_point: ckb_types::packed::OutPoint = cell.out_point.into();
                OutPoint::new_unchecked(out_point.as_bytes())
            };
            let output = {
                let output: ckb_types::packed::CellOutput = cell.output.into();
                CellOutput::new_unchecked(output.as_bytes())
            };
            let data = cell.output_data.into_bytes();

            CellInfo {
                out_point,
                output,
                data,
            }
        };

        Ok(stake_cell.map(fetch_cell_info))
    }

    pub async fn query_stake_cells_by_owner_lock_hashes(
        &self,
        owner_lock_hashes: impl Iterator<Item = [u8; 32]>,
    ) -> Result<Vec<CellInfo>> {
        let lock = Script::new_builder()
            .code_hash(self.rollup_context.rollup_config.stake_script_type_hash())
            .hash_type(ScriptHashType::Type.into())
            .args(self.rollup_context.rollup_script_hash.as_slice().pack())
            .build();

        let search_key = SearchKey {
            script: {
                let lock = ckb_types::packed::Script::new_unchecked(lock.as_bytes());
                lock.into()
            },
            script_type: ScriptType::Lock,
            filter: Some(SearchKeyFilter {
                script: None,
                output_data_len_range: None,
                output_capacity_range: None,
                block_range: None,
            }),
        };
        let order = Order::Desc;
        let limit = Uint32::from(DEFAULT_QUERY_LIMIT as u32);

        let owner_lock_hashes: HashSet<[u8; 32]> = owner_lock_hashes.collect();
        let mut collected_owners = HashSet::new();
        let mut collected_cells = Vec::new();
        let mut cursor = None;

        while collected_owners.len() != owner_lock_hashes.len() {
            let cells: Pagination<Cell> = to_result(
                self.indexer_client
                    .request(
                        "get_cells",
                        Some(ClientParams::Array(vec![
                            json!(search_key),
                            json!(order),
                            json!(limit),
                            json!(cursor),
                        ])),
                    )
                    .await?,
            )?;

            if cells.last_cursor.is_empty() {
                return Err(anyhow!("no all reward stake cells found"));
            }
            cursor = Some(cells.last_cursor);

            for cell in cells.objects.into_iter() {
                let args = cell.output.lock.args.clone().into_bytes();
                let stake_lock_args = match StakeLockArgsReader::verify(&args[32..], false) {
                    Ok(()) => StakeLockArgs::new_unchecked(args.slice(32..)),
                    Err(_) => continue,
                };

                let owner_lock_hash: [u8; 32] = stake_lock_args.owner_lock_hash().unpack();
                if owner_lock_hashes.contains(&owner_lock_hash)
                    && !collected_owners.contains(&owner_lock_hash)
                {
                    collected_owners.insert(owner_lock_hash);
                    collected_cells.push(to_cell_info(cell));
                }
            }
        }

        Ok(collected_cells)
    }

    pub async fn query_custodian_cells_by_block_hashes(
        &self,
        block_hashes: &HashSet<H256>,
    ) -> Result<(Vec<CellInfo>, HashSet<H256>)> {
        let rollup_context = &self.rollup_context;

        let custodian_lock = Script::new_builder()
            .code_hash(rollup_context.rollup_config.custodian_script_type_hash())
            .hash_type(ScriptHashType::Type.into())
            .args(rollup_context.rollup_script_hash.as_slice().pack())
            .build();

        let search_key = SearchKey {
            script: ckb_types::packed::Script::new_unchecked(custodian_lock.as_bytes()).into(),
            script_type: ScriptType::Lock,
            filter: None,
        };
        let order = Order::Desc;
        let limit = Uint32::from(DEFAULT_QUERY_LIMIT as u32);

        let mut collected = vec![];
        let mut collected_block_hashes = HashSet::new();
        let mut cursor = None;

        while collected.is_empty() {
            let cells: Pagination<Cell> = to_result(
                self.indexer_client
                    .request(
                        "get_cells",
                        Some(ClientParams::Array(vec![
                            json!(search_key),
                            json!(order),
                            json!(limit),
                            json!(cursor),
                        ])),
                    )
                    .await?,
            )?;

            if cells.last_cursor.is_empty() {
                return Ok((collected, collected_block_hashes));
            }
            cursor = Some(cells.last_cursor);

            for cell in cells.objects.into_iter() {
                let args = cell.output.lock.args.clone().into_bytes();
                let custodian_lock_args = match CustodianLockArgsReader::verify(&args[32..], false)
                {
                    Ok(()) => CustodianLockArgs::new_unchecked(args.slice(32..)),
                    Err(_) => continue,
                };

                let deposit_block_hash: H256 = custodian_lock_args.deposit_block_hash().unpack();
                if !block_hashes.contains(&deposit_block_hash) {
                    continue;
                }

                collected.push(to_cell_info(cell));
                collected_block_hashes.insert(deposit_block_hash);
            }
        }

        Ok((collected, collected_block_hashes))
    }

    pub async fn query_finalized_custodian_cells(
        &self,
        withdrawals_amount: &WithdrawalsAmount,
        last_finalized_block_number: u64,
    ) -> Result<CollectedCustodianCells> {
        let rollup_context = &self.rollup_context;

        let parse_sudt_amount = |cell: &Cell| -> Result<u128> {
            if cell.output.type_.is_none() {
                return Err(anyhow!("no a sudt cell"));
            }

            gw_types::packed::Uint128::from_slice(&cell.output_data.as_bytes())
                .map(|a| a.unpack())
                .map_err(|e| anyhow!("invalid sudt amount {}", e))
        };

        let custodian_lock = Script::new_builder()
            .code_hash(rollup_context.rollup_config.custodian_script_type_hash())
            .hash_type(ScriptHashType::Type.into())
            .args(rollup_context.rollup_script_hash.as_slice().pack())
            .build();

        let search_key = SearchKey {
            script: ckb_types::packed::Script::new_unchecked(custodian_lock.as_bytes()).into(),
            script_type: ScriptType::Lock,
            filter: None,
        };
        let order = Order::Desc;
        let limit = Uint32::from(DEFAULT_QUERY_LIMIT as u32);

        let mut collected = CollectedCustodianCells::default();
        let mut collected_fullfilled_sudt = HashSet::new();
        let mut cursor = None;

        while collected.capacity < withdrawals_amount.capacity
            || collected_fullfilled_sudt.len() < withdrawals_amount.sudt.len()
        {
            let cells: Pagination<Cell> = to_result(
                self.indexer_client
                    .request(
                        "get_cells",
                        Some(ClientParams::Array(vec![
                            json!(search_key),
                            json!(order),
                            json!(limit),
                            json!(cursor),
                        ])),
                    )
                    .await?,
            )?;

            if cells.last_cursor.is_empty() {
                return Err(anyhow!("no enough finalized custodians"));
            }
            cursor = Some(cells.last_cursor);

            for cell in cells.objects.into_iter() {
                let args = cell.output.lock.args.clone().into_bytes();
                let custodian_lock_args = match CustodianLockArgsReader::verify(&args[32..], false)
                {
                    Ok(()) => CustodianLockArgs::new_unchecked(args.slice(32..)),
                    Err(_) => continue,
                };

                if custodian_lock_args.deposit_block_number().unpack() > last_finalized_block_number
                {
                    continue;
                }

                // Collect sudt
                if let Some(json_script) = cell.output.type_.clone() {
                    let sudt_type_script = {
                        let script = ckb_types::packed::Script::from(json_script);
                        Script::new_unchecked(script.as_bytes())
                    };

                    // Invalid custodian type script
                    let l1_sudt_script_type_hash =
                        rollup_context.rollup_config.l1_sudt_script_type_hash();
                    if sudt_type_script.code_hash() != l1_sudt_script_type_hash
                        || sudt_type_script.hash_type() != ScriptHashType::Type.into()
                    {
                        continue;
                    }

                    let sudt_type_hash = sudt_type_script.hash();
                    if sudt_type_hash != CKB_SUDT_SCRIPT_ARGS {
                        // Already collected enough sudt amount
                        if collected_fullfilled_sudt.contains(&sudt_type_hash) {
                            continue;
                        }

                        // Not targed withdrawal sudt
                        let withdrawal_amount = match withdrawals_amount.sudt.get(&sudt_type_hash) {
                            Some(amount) => amount,
                            None => continue,
                        };

                        let sudt_amount = match parse_sudt_amount(&cell) {
                            Ok(amount) => amount,
                            Err(_) => {
                                log::error!("invalid sudt amount, out_point: {:?}", cell.out_point);
                                continue;
                            }
                        };

                        let (collected_amount, type_script) = {
                            let sudt = collected.sudt.entry(sudt_type_hash);
                            sudt.or_insert((0, Script::default()))
                        };
                        *collected_amount = collected_amount.saturating_add(sudt_amount);
                        *type_script = sudt_type_script;

                        if *collected_amount >= *withdrawal_amount {
                            collected_fullfilled_sudt.insert(sudt_type_hash);
                        }
                    }
                }

                // Collect capacity
                let out_point = {
                    let out_point: ckb_types::packed::OutPoint = cell.out_point.into();
                    OutPoint::new_unchecked(out_point.as_bytes())
                };

                let output = {
                    let output: ckb_types::packed::CellOutput = cell.output.into();
                    CellOutput::new_unchecked(output.as_bytes())
                };

                collected.capacity = collected
                    .capacity
                    .saturating_add(output.capacity().unpack() as u128);

                let info = CellInfo {
                    out_point,
                    output,
                    data: cell.output_data.into_bytes(),
                };

                collected.cells_info.push(info);
            }
        }

        Ok(collected)
    }

    pub async fn query_verified_custodian_type_script(
        &self,
        sudt_script_hash: &[u8; 32],
    ) -> Result<Option<Script>> {
        let rollup_context = &self.rollup_context;

        let custodian_lock = Script::new_builder()
            .code_hash(rollup_context.rollup_config.custodian_script_type_hash())
            .hash_type(ScriptHashType::Type.into())
            .args(rollup_context.rollup_script_hash.as_slice().pack())
            .build();

        let l1_sudt_type = Script::new_builder()
            .code_hash(rollup_context.rollup_config.l1_sudt_script_type_hash())
            .hash_type(ScriptHashType::Type.into())
            .build();

        let search_key = SearchKey {
            script: ckb_types::packed::Script::new_unchecked(custodian_lock.as_bytes()).into(),
            script_type: ScriptType::Lock,
            filter: Some(SearchKeyFilter {
                script: Some(
                    ckb_types::packed::Script::new_unchecked(l1_sudt_type.as_bytes()).into(),
                ),
                output_data_len_range: None,
                output_capacity_range: None,
                block_range: None,
            }),
        };
        let order = Order::Desc;
        let limit = Uint32::from(DEFAULT_QUERY_LIMIT as u32);

        let mut cursor = None;
        loop {
            let cells: Pagination<Cell> = to_result(
                self.indexer_client
                    .request(
                        "get_cells",
                        Some(ClientParams::Array(vec![
                            json!(search_key),
                            json!(order),
                            json!(limit),
                            json!(cursor),
                        ])),
                    )
                    .await?,
            )?;

            if cells.last_cursor.is_empty() {
                return Ok(None);
            }
            cursor = Some(cells.last_cursor);

            for cell in cells.objects.into_iter() {
                let args = cell.output.lock.args.clone().into_bytes();
                if CustodianLockArgsReader::verify(&args[32..], false).is_err() {
                    continue;
                }

                let sudt_type_script = match cell.output.type_.clone() {
                    Some(json_script) => {
                        let script = ckb_types::packed::Script::from(json_script);
                        Script::new_unchecked(script.as_bytes())
                    }
                    None => continue,
                };

                if sudt_script_hash == &sudt_type_script.hash() {
                    return Ok(Some(sudt_type_script));
                }
            }
        }
    }

    pub async fn query_withdrawal_cells_by_block_hashes(
        &self,
        block_hashes: &HashSet<H256>,
    ) -> Result<(Vec<CellInfo>, HashSet<H256>)> {
        let rollup_context = &self.rollup_context;

        let withdrawal_lock = Script::new_builder()
            .code_hash(rollup_context.rollup_config.withdrawal_script_type_hash())
            .hash_type(ScriptHashType::Type.into())
            .args(rollup_context.rollup_script_hash.as_slice().pack())
            .build();

        let search_key = SearchKey {
            script: ckb_types::packed::Script::new_unchecked(withdrawal_lock.as_bytes()).into(),
            script_type: ScriptType::Lock,
            filter: None,
        };
        let order = Order::Desc;
        let limit = Uint32::from(DEFAULT_QUERY_LIMIT as u32);

        let mut collected = vec![];
        let mut collected_block_hashes = HashSet::new();
        let mut cursor = None;

        while collected.is_empty() {
            let cells: Pagination<Cell> = to_result(
                self.indexer_client
                    .request(
                        "get_cells",
                        Some(ClientParams::Array(vec![
                            json!(search_key),
                            json!(order),
                            json!(limit),
                            json!(cursor),
                        ])),
                    )
                    .await?,
            )?;

            if cells.last_cursor.is_empty() {
                return Ok((collected, collected_block_hashes));
            }
            cursor = Some(cells.last_cursor);

            for cell in cells.objects.into_iter() {
                let args = cell.output.lock.args.clone().into_bytes();
                let withdrawal_lock_args =
                    match WithdrawalLockArgsReader::verify(&args[32..], false) {
                        Ok(()) => WithdrawalLockArgs::new_unchecked(args.slice(32..)),
                        Err(_) => continue,
                    };

                let withdrawal_block_hash: H256 =
                    withdrawal_lock_args.withdrawal_block_hash().unpack();
                if !block_hashes.contains(&withdrawal_block_hash) {
                    continue;
                }

                collected.push(to_cell_info(cell));
                collected_block_hashes.insert(withdrawal_block_hash);
            }
        }

        Ok((collected, collected_block_hashes))
    }

    pub async fn query_verifier_cell(
        &self,
        allowed_script_type_hash: [u8; 32],
        owner_lock_hash: [u8; 32],
    ) -> Result<Option<CellInfo>> {
        let lock = Script::new_builder()
            .code_hash(allowed_script_type_hash.pack())
            .hash_type(ScriptHashType::Type.into())
            .args(self.rollup_context.rollup_script_hash.as_slice().pack())
            .build();

        let search_key = SearchKey {
            script: {
                let lock = ckb_types::packed::Script::new_unchecked(lock.as_bytes());
                lock.into()
            },
            script_type: ScriptType::Lock,
            filter: Some(SearchKeyFilter {
                script: None,
                output_data_len_range: None,
                output_capacity_range: None,
                block_range: None,
            }),
        };
        let order = Order::Desc;
        let limit = Uint32::from(DEFAULT_QUERY_LIMIT as u32);

        let mut verifier_cell = None;
        let mut cursor = None;

        while verifier_cell.is_none() {
            let cells: Pagination<Cell> = to_result(
                self.indexer_client
                    .request(
                        "get_cells",
                        Some(ClientParams::Array(vec![
                            json!(search_key),
                            json!(order),
                            json!(limit),
                            json!(cursor),
                        ])),
                    )
                    .await?,
            )?;

            if cells.last_cursor.is_empty() {
                log::debug!(
                    "no verifier cell for script type hash {:?}",
                    allowed_script_type_hash
                );
                return Ok(None);
            }
            cursor = Some(cells.last_cursor);

            verifier_cell = cells.objects.into_iter().find_map(|cell| {
                if cell.output_data.len() >= 32
                    && cell.output_data.as_bytes()[0..32] == owner_lock_hash
                {
                    Some(to_cell_info(cell))
                } else {
                    None
                }
            });
        }

        Ok(verifier_cell)
    }

    pub async fn get_block(
        &self,
        block_hash: H256,
    ) -> Result<Option<ckb_jsonrpc_types::BlockView>> {
        let block: Option<ckb_jsonrpc_types::BlockView> = to_result(
            self.ckb_client
                .request(
                    "get_block",
                    Some(ClientParams::Array(vec![json!(to_jsonh256(block_hash))])),
                )
                .await?,
        )?;

        Ok(block)
    }

    pub async fn get_header(
        &self,
        block_hash: H256,
    ) -> Result<Option<ckb_jsonrpc_types::HeaderView>> {
        let block: Option<ckb_jsonrpc_types::HeaderView> = to_result(
            self.ckb_client
                .request(
                    "get_header",
                    Some(ClientParams::Array(vec![json!(to_jsonh256(block_hash))])),
                )
                .await?,
        )?;

        Ok(block)
    }

    pub async fn get_transaction_block_hash(&self, tx_hash: H256) -> Result<Option<[u8; 32]>> {
        let tx_with_status: Option<ckb_jsonrpc_types::TransactionWithStatus> = to_result(
            self.ckb_client
                .request(
                    "get_transaction",
                    Some(ClientParams::Array(vec![json!(to_jsonh256(tx_hash))])),
                )
                .await?,
        )?;

        match tx_with_status {
            Some(tx_with_status) => {
                let block_hash: ckb_fixed_hash::H256 = {
                    let status = tx_with_status.tx_status;
                    status.block_hash.ok_or_else(|| anyhow!("no tx block hash"))
                }?;
                Ok(Some(block_hash.into()))
            }
            None => Ok(None),
        }
    }

    pub async fn get_transaction_block_number(&self, tx_hash: H256) -> Result<Option<u64>> {
        match self.get_transaction_block_hash(tx_hash).await? {
            Some(block_hash) => {
                let block = self.get_block(block_hash.into()).await?;
                Ok(block.map(|b| b.header.inner.number.value()))
            }
            None => Ok(None),
        }
    }

    pub async fn get_transaction(&self, tx_hash: H256) -> Result<Option<Transaction>> {
        let tx_with_status: Option<ckb_jsonrpc_types::TransactionWithStatus> = to_result(
            self.ckb_client
                .request(
                    "get_transaction",
                    Some(ClientParams::Array(vec![json!(to_jsonh256(tx_hash))])),
                )
                .await?,
        )?;
        Ok(tx_with_status.map(|tx_with_status| {
            let tx: ckb_types::packed::Transaction = tx_with_status.transaction.inner.into();
            Transaction::new_unchecked(tx.as_bytes())
        }))
    }

    pub async fn get_transaction_status(&self, tx_hash: H256) -> Result<Option<TxStatus>> {
        let tx_with_status: Option<ckb_jsonrpc_types::TransactionWithStatus> = to_result(
            self.ckb_client
                .request(
                    "get_transaction",
                    Some(ClientParams::Array(vec![json!(to_jsonh256(tx_hash))])),
                )
                .await?,
        )?;

        Ok(
            tx_with_status.map(|tx_with_status| match tx_with_status.tx_status.status {
                ckb_jsonrpc_types::Status::Pending => TxStatus::Pending,
                ckb_jsonrpc_types::Status::Committed => TxStatus::Committed,
                ckb_jsonrpc_types::Status::Proposed => TxStatus::Proposed,
            }),
        )
    }

    pub async fn send_transaction(&self, tx: Transaction) -> Result<H256> {
        let tx: ckb_jsonrpc_types::Transaction = {
            let tx = ckb_types::packed::Transaction::new_unchecked(tx.as_bytes());
            tx.into()
        };
        let tx_hash: ckb_types::H256 = to_result(
            self.ckb_client
                .request(
                    "send_transaction",
                    Some(ClientParams::Array(vec![json!(tx)])),
                )
                .await?,
        )?;
        Ok(to_h256(tx_hash))
    }

    pub async fn get_ckb_version(&self) -> Result<String> {
        let node: ckb_jsonrpc_types::LocalNode =
            to_result(self.ckb_client.request("local_node_info", None).await?)?;
        Ok(node.version)
    }

    pub async fn dry_run_transaction(&self, tx: Transaction) -> Result<u64> {
        let tx: ckb_jsonrpc_types::Transaction = {
            let tx = ckb_types::packed::Transaction::new_unchecked(tx.as_bytes());
            tx.into()
        };
        let dry_run_result: ckb_jsonrpc_types::DryRunResult = to_result(
            self.ckb_client
                .request(
                    "dry_run_transaction",
                    Some(ClientParams::Array(vec![json!(tx)])),
                )
                .await?,
        )?;
        Ok(dry_run_result.cycles.into())
    }
}
