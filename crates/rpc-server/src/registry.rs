use anyhow::Result;
use async_trait::async_trait;
use ckb_types::prelude::{Builder, Entity};
use gw_common::{state::State, H256};
use gw_generator::{sudt::build_l2_sudt_script, Generator};
use gw_jsonrpc_types::{
    blockchain::Script,
    ckb_jsonrpc_types::{JsonBytes, Uint128, Uint32},
    godwoken::{GlobalState, L2BlockStatus, L2BlockView, L2BlockWithStatus, RunResult, TxReceipt},
    test_mode::{ShouldProduceBlock, TestModePayload},
};
use gw_store::{
    state_db::{CheckPoint, StateDBMode, StateDBTransaction, SubState},
    Store,
};
use gw_traits::CodeStore;
use gw_types::{
    packed::{self, BlockInfo},
    prelude::*,
};
use jsonrpc_v2::{Data, Error as RpcError, MapRouter, Params, Server, Server as JsonrpcServer};
use parking_lot::Mutex;
use std::sync::Arc;

// type alias
type RPCServer = Arc<Server<MapRouter>>;
type MemPool = Mutex<gw_mem_pool::pool::MemPool>;
type AccountID = Uint32;
type JsonH256 = ckb_fixed_hash::H256;
type BoxedTestsRPCImpl = Box<dyn TestModeRPC + Send + Sync>;
type GwUint64 = gw_jsonrpc_types::ckb_jsonrpc_types::Uint64;

const HEADER_NOT_FOUND_ERR_CODE: i64 = -32000;

#[async_trait]
pub trait TestModeRPC {
    async fn get_global_state(&self) -> Result<GlobalState>;
    async fn produce_block(&self, payload: TestModePayload) -> Result<()>;
    async fn should_produce_block(&self) -> Result<ShouldProduceBlock>;
}

fn to_h256(v: JsonH256) -> H256 {
    let h: [u8; 32] = v.into();
    h.into()
}

fn to_jsonh256(v: H256) -> JsonH256 {
    let h: [u8; 32] = v.into();
    h.into()
}

pub struct Registry {
    generator: Arc<Generator>,
    mem_pool: Arc<MemPool>,
    store: Store,
    tests_rpc_impl: Option<Arc<BoxedTestsRPCImpl>>,
}

impl Registry {
    pub fn new<T>(
        store: Store,
        mem_pool: Arc<MemPool>,
        generator: Arc<Generator>,
        tests_rpc_impl: Option<Box<T>>,
    ) -> Self
    where
        T: TestModeRPC + Send + Sync + 'static,
    {
        Self {
            mem_pool,
            store,
            generator,
            tests_rpc_impl: tests_rpc_impl
                .map(|r| Arc::new(r as Box<dyn TestModeRPC + Sync + Send + 'static>)),
        }
    }

    pub fn build_rpc_server(self) -> Result<RPCServer> {
        let mut server = JsonrpcServer::new();

        server = server
            .with_data(Data(self.mem_pool.clone()))
            .with_data(Data(self.generator.clone()))
            .with_data(Data::new(self.store))
            .with_method("ping", ping)
            .with_method("get_tip_block_hash", get_tip_block_hash)
            .with_method("get_block_hash", get_block_hash)
            .with_method("get_block", get_block)
            .with_method("get_block_by_number", get_block_by_number)
            .with_method("get_balance", get_balance)
            .with_method("get_storage_at", get_storage_at)
            .with_method(
                "get_account_id_by_script_hash",
                get_account_id_by_script_hash,
            )
            .with_method("get_nonce", get_nonce)
            .with_method("get_script", get_script)
            .with_method("get_script_hash", get_script_hash)
            .with_method(
                "get_script_hash_by_short_address",
                get_script_hash_by_short_address,
            )
            .with_method("get_data", get_data)
            .with_method("get_transaction_receipt", get_transaction_receipt)
            .with_method("execute_l2transaction", execute_l2transaction)
            .with_method("execute_raw_l2transaction", execute_raw_l2transaction)
            .with_method("submit_l2transaction", submit_l2transaction)
            .with_method("submit_withdrawal_request", submit_withdrawal_request)
            .with_method("compute_l2_sudt_script_hash", compute_l2_sudt_script_hash);

        // Tests
        if let Some(tests_rpc_impl) = self.tests_rpc_impl {
            server = server
                .with_data(Data(Arc::clone(&tests_rpc_impl)))
                .with_method("tests_produce_block", tests_produce_block)
                .with_method("tests_should_produce_block", tests_should_produce_block)
                .with_method("tests_get_global_state", tests_get_global_state);
        }

        Ok(server.finish())
    }
}

async fn ping() -> Result<String> {
    Ok("pong".to_string())
}

async fn get_block(
    Params((block_hash,)): Params<(JsonH256,)>,
    store: Data<Store>,
) -> Result<Option<L2BlockWithStatus>> {
    let block_hash = to_h256(block_hash);
    let db = store.begin_transaction();
    let block_opt = db.get_block(&block_hash)?.map(|block| {
        let block_view: L2BlockView = block.into();
        block_view
    });
    let l2_block_status = if H256::zero() != db.reverted_block_smt()?.get(&block_hash)? {
        L2BlockStatus::Reverted
    } else {
        // let tip_block = db.get_last_valid_tip_block();
        // let block_number = db.get_block_number(&block_hash)?;
        // L2BlockStatus::Unfinalized if block_number <= tip_block_number - NUMBER_OF_CONFIRMATION

        L2BlockStatus::Unfinalized
    };
    Ok(block_opt.map(|l2_block_view| L2BlockWithStatus {
        l2_block_view,
        l2_block_status,
    }))
}

async fn get_block_by_number(
    Params((block_number,)): Params<(gw_jsonrpc_types::ckb_jsonrpc_types::Uint64,)>,
    store: Data<Store>,
) -> Result<Option<L2BlockView>> {
    let block_number = block_number.value();
    let db = store.begin_transaction();
    let block_hash = match db.get_block_hash_by_number(block_number)? {
        Some(hash) => hash,
        None => return Ok(None),
    };
    let block_opt = db.get_block(&block_hash)?.map(|block| {
        let block_view: L2BlockView = block.into();
        block_view
    });
    Ok(block_opt)
}

async fn get_block_hash(
    Params((block_number,)): Params<(gw_jsonrpc_types::ckb_jsonrpc_types::Uint64,)>,
    store: Data<Store>,
) -> Result<Option<JsonH256>> {
    let block_number = block_number.value();
    let db = store.begin_transaction();
    let hash_opt = db.get_block_hash_by_number(block_number)?.map(to_jsonh256);
    Ok(hash_opt)
}

async fn get_tip_block_hash(store: Data<Store>) -> Result<JsonH256> {
    let tip_block_hash = store.get_tip_block_hash()?;
    Ok(to_jsonh256(tip_block_hash))
}

async fn get_transaction_receipt(
    Params((tx_hash,)): Params<(JsonH256,)>,
    store: Data<Store>,
) -> Result<Option<TxReceipt>> {
    let tx_hash = to_h256(tx_hash);
    let db = store.begin_transaction();
    let receipt_opt = db.get_transaction_receipt(&tx_hash)?.map(|receipt| {
        let receipt: TxReceipt = receipt.into();
        receipt
    });
    Ok(receipt_opt)
}

async fn execute_l2transaction(
    Params((l2tx,)): Params<(JsonBytes,)>,
    mem_pool: Data<MemPool>,
    store: Data<Store>,
) -> Result<RunResult> {
    let l2tx_bytes = l2tx.into_bytes();
    let tx = packed::L2Transaction::from_slice(&l2tx_bytes)?;

    let raw_block = store.get_tip_block()?.raw();
    let block_producer_id = raw_block.block_producer_id();
    let timestamp = raw_block.timestamp();
    let number = {
        let number: u64 = raw_block.number().unpack();
        number.saturating_add(1)
    };

    let block_info = BlockInfo::new_builder()
        .block_producer_id(block_producer_id)
        .timestamp(timestamp)
        .number(number.pack())
        .build();

    let run_result: RunResult = mem_pool.lock().execute_transaction(tx, &block_info)?.into();
    Ok(run_result)
}

// raw_l2tx, block_number
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum ExecuteRawL2TransactionParams {
    Tip((JsonBytes,)),
    Number((JsonBytes, Option<GwUint64>)),
}

async fn execute_raw_l2transaction(
    Params(params): Params<ExecuteRawL2TransactionParams>,
    mem_pool: Data<MemPool>,
    store: Data<Store>,
) -> Result<RunResult, RpcError> {
    let (raw_l2tx, block_number) = match params {
        ExecuteRawL2TransactionParams::Tip(p) => (p.0, None),
        ExecuteRawL2TransactionParams::Number(p) => p,
    };

    let raw_l2tx_bytes = raw_l2tx.into_bytes();
    let raw_l2tx = packed::RawL2Transaction::from_slice(&raw_l2tx_bytes)?;

    let db = store.begin_transaction();
    let block_number = match block_number {
        Some(num) => num.value(),
        None => db.get_tip_block()?.raw().number().unpack(),
    };
    let not_found_err = Err(RpcError::Provided {
        code: HEADER_NOT_FOUND_ERR_CODE,
        message: "header not found",
    });
    let block_hash = match db.get_block_hash_by_number(block_number)? {
        Some(block_hash) => block_hash,
        None => return not_found_err,
    };

    let raw_block = match store.get_block(&block_hash)? {
        Some(block) => block.raw(),
        None => return not_found_err,
    };
    let block_producer_id = raw_block.block_producer_id();
    let timestamp = raw_block.timestamp();
    let number = {
        let number: u64 = raw_block.number().unpack();
        number.saturating_add(1)
    };

    let block_info = BlockInfo::new_builder()
        .block_producer_id(block_producer_id)
        .timestamp(timestamp)
        .number(number.pack())
        .build();

    let run_result: RunResult = mem_pool
        .lock()
        .execute_raw_transaction(raw_l2tx, &block_info, block_number)?
        .into();
    Ok(run_result)
}

async fn submit_l2transaction(
    Params((l2tx,)): Params<(JsonBytes,)>,
    mem_pool: Data<MemPool>,
) -> Result<JsonH256> {
    let l2tx_bytes = l2tx.into_bytes();
    let tx = packed::L2Transaction::from_slice(&l2tx_bytes)?;
    let tx_hash = to_jsonh256(tx.hash().into());
    mem_pool.lock().push_transaction(tx)?;
    Ok(tx_hash)
}

async fn submit_withdrawal_request(
    Params((withdrawal_request,)): Params<(JsonBytes,)>,
    mem_pool: Data<MemPool>,
) -> Result<()> {
    let withdrawal_bytes = withdrawal_request.into_bytes();
    let withdrawal = packed::WithdrawalRequest::from_slice(&withdrawal_bytes)?;

    mem_pool.lock().push_withdrawal_request(withdrawal)?;
    Ok(())
}

// short_address, sudt_id, block_number
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum GetBalanceParams {
    Tip((JsonBytes, AccountID)),
    Number((JsonBytes, AccountID, Option<GwUint64>)),
}

async fn get_balance(
    Params(params): Params<GetBalanceParams>,
    store: Data<Store>,
) -> Result<Uint128, RpcError> {
    let (short_address, sudt_id, block_number) = match params {
        GetBalanceParams::Tip(p) => (p.0, p.1, None),
        GetBalanceParams::Number(p) => p,
    };

    let db = store.begin_transaction();
    let tip_block_number = db.get_tip_block()?.raw().number().unpack();
    let block_number = match block_number {
        Some(num) => num.value(),
        None => tip_block_number,
    };
    if block_number > tip_block_number {
        return Err(RpcError::Provided {
            code: HEADER_NOT_FOUND_ERR_CODE,
            message: "header not found",
        });
    }
    let state_db = StateDBTransaction::from_checkpoint(
        &db,
        CheckPoint::new(block_number, SubState::Block),
        StateDBMode::ReadOnly,
    )?;

    let tree = state_db.account_state_tree()?;
    let balance = tree.get_sudt_balance(sudt_id.into(), short_address.as_bytes())?;
    Ok(balance.into())
}

// account_id, key, block_number
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum GetStorageAtParams {
    Tip((AccountID, JsonH256)),
    Number((AccountID, JsonH256, Option<GwUint64>)),
}

async fn get_storage_at(
    Params(params): Params<GetStorageAtParams>,
    store: Data<Store>,
) -> Result<JsonH256, RpcError> {
    let (account_id, key, block_number) = match params {
        GetStorageAtParams::Tip(p) => (p.0, p.1, None),
        GetStorageAtParams::Number(p) => p,
    };

    let db = store.begin_transaction();
    let tip_block_number = db.get_tip_block()?.raw().number().unpack();
    let block_number = match block_number {
        Some(num) => num.value(),
        None => tip_block_number,
    };
    if block_number > tip_block_number {
        return Err(RpcError::Provided {
            code: HEADER_NOT_FOUND_ERR_CODE,
            message: "header not found",
        });
    }
    let state_db = StateDBTransaction::from_checkpoint(
        &db,
        CheckPoint::new(block_number, SubState::Block),
        StateDBMode::ReadOnly,
    )?;

    let tree = state_db.account_state_tree()?;
    let key: H256 = to_h256(key);
    let value = tree.get_value(account_id.into(), &key)?;

    let json_value = to_jsonh256(value);
    Ok(json_value)
}

async fn get_account_id_by_script_hash(
    Params((script_hash,)): Params<(JsonH256,)>,
    store: Data<Store>,
) -> Result<Option<AccountID>> {
    let db = store.begin_transaction();
    let tip_hash = db.get_tip_block_hash()?;
    let state_db = StateDBTransaction::from_checkpoint(
        &db,
        CheckPoint::from_block_hash(&db, tip_hash, SubState::Block)?,
        StateDBMode::ReadOnly,
    )?;
    let tree = state_db.account_state_tree()?;

    let script_hash = to_h256(script_hash);

    let account_id_opt = tree
        .get_account_id_by_script_hash(&script_hash)?
        .map(Into::into);

    Ok(account_id_opt)
}

// account_id, block_number
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum GetNonceParams {
    Tip((AccountID,)),
    Number((AccountID, Option<GwUint64>)),
}

async fn get_nonce(
    Params(params): Params<GetNonceParams>,
    store: Data<Store>,
) -> Result<Uint32, RpcError> {
    let (account_id, block_number) = match params {
        GetNonceParams::Tip(p) => (p.0, None),
        GetNonceParams::Number(p) => p,
    };

    let db = store.begin_transaction();
    let tip_block_number = db.get_tip_block()?.raw().number().unpack();
    let block_number = match block_number {
        Some(num) => num.value(),
        None => tip_block_number,
    };
    if block_number > tip_block_number {
        return Err(RpcError::Provided {
            code: HEADER_NOT_FOUND_ERR_CODE,
            message: "header not found",
        });
    }
    let state_db = StateDBTransaction::from_checkpoint(
        &db,
        CheckPoint::new(block_number, SubState::Block),
        StateDBMode::ReadOnly,
    )?;
    let tree = state_db.account_state_tree()?;

    let nonce = tree.get_nonce(account_id.into())?;

    Ok(nonce.into())
}

async fn get_script(
    Params((script_hash,)): Params<(JsonH256,)>,
    store: Data<Store>,
) -> Result<Option<Script>> {
    let db = store.begin_transaction();
    let tip_hash = db.get_tip_block_hash()?;
    let state_db = StateDBTransaction::from_checkpoint(
        &db,
        CheckPoint::from_block_hash(&db, tip_hash, SubState::Block)?,
        StateDBMode::ReadOnly,
    )?;
    let tree = state_db.account_state_tree()?;

    let script_hash = to_h256(script_hash);
    let script_opt = tree.get_script(&script_hash).map(Into::into);

    Ok(script_opt)
}

async fn get_script_hash(
    Params((account_id,)): Params<(AccountID,)>,
    store: Data<Store>,
) -> Result<JsonH256> {
    let db = store.begin_transaction();
    let tip_hash = db.get_tip_block_hash()?;
    let state_db = StateDBTransaction::from_checkpoint(
        &db,
        CheckPoint::from_block_hash(&db, tip_hash, SubState::Block)?,
        StateDBMode::ReadOnly,
    )?;
    let tree = state_db.account_state_tree()?;

    let script_hash = tree.get_script_hash(account_id.into())?;
    Ok(to_jsonh256(script_hash))
}

async fn get_script_hash_by_short_address(
    Params((short_address,)): Params<(JsonBytes,)>,
    store: Data<Store>,
) -> Result<Option<JsonH256>> {
    let db = store.begin_transaction();
    let tip_hash = db.get_tip_block_hash()?;
    let state_db = StateDBTransaction::from_checkpoint(
        &db,
        CheckPoint::from_block_hash(&db, tip_hash, SubState::Block)?,
        StateDBMode::ReadOnly,
    )?;
    let tree = state_db.account_state_tree()?;
    let script_hash_opt = tree.get_script_hash_by_short_address(&short_address.into_bytes());
    Ok(script_hash_opt.map(to_jsonh256))
}

// data_hash, block_number
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum GetDataParams {
    Tip((JsonH256,)),
    Number((JsonH256, Option<GwUint64>)),
}

async fn get_data(
    Params(params): Params<GetDataParams>,
    store: Data<Store>,
) -> Result<Option<JsonBytes>, RpcError> {
    let (data_hash, block_number) = match params {
        GetDataParams::Tip(p) => (p.0, None),
        GetDataParams::Number(p) => p,
    };

    let db = store.begin_transaction();
    let tip_block_number = db.get_tip_block()?.raw().number().unpack();
    let block_number = match block_number {
        Some(num) => num.value(),
        None => tip_block_number,
    };
    if block_number > tip_block_number {
        return Err(RpcError::Provided {
            code: HEADER_NOT_FOUND_ERR_CODE,
            message: "header not found",
        });
    }
    let state_db = StateDBTransaction::from_checkpoint(
        &db,
        CheckPoint::new(block_number, SubState::Block),
        StateDBMode::ReadOnly,
    )?;
    let tree = state_db.account_state_tree()?;

    let data_opt = tree
        .get_data(&to_h256(data_hash))
        .map(JsonBytes::from_bytes);

    Ok(data_opt)
}

async fn compute_l2_sudt_script_hash(
    Params((l1_sudt_script_hash,)): Params<(JsonH256,)>,
    generator: Data<Generator>,
) -> Result<JsonH256> {
    let l2_sudt_script =
        build_l2_sudt_script(generator.rollup_context(), &to_h256(l1_sudt_script_hash));
    Ok(to_jsonh256(l2_sudt_script.hash().into()))
}

async fn tests_produce_block(
    Params((payload,)): Params<(TestModePayload,)>,
    tests_rpc_impl: Data<BoxedTestsRPCImpl>,
) -> Result<()> {
    tests_rpc_impl.produce_block(payload).await
}

async fn tests_get_global_state(tests_rpc_impl: Data<BoxedTestsRPCImpl>) -> Result<GlobalState> {
    tests_rpc_impl.get_global_state().await
}

async fn tests_should_produce_block(
    tests_rpc_impl: Data<BoxedTestsRPCImpl>,
) -> Result<ShouldProduceBlock> {
    tests_rpc_impl.should_produce_block().await
}
