use crate::{
    helper::{account_id_to_eth_address, parse_log, GwLog, PolyjuiceArgs},
    types::{
        Block as Web3Block, Log as Web3Log, Transaction as Web3Transaction,
        TransactionWithLogs as Web3TransactionWithLogs,
    },
};
use ckb_hash::blake2b_256;
use ckb_types::{packed::WitnessArgs, H256};
use faster_hex;
use gw_common::builtins::CKB_SUDT_ACCOUNT_ID;
use gw_common::state::State;
use gw_store::{
    state_db::{StateDBTransaction, StateDBVersion},
    Store,
};
use gw_traits::CodeStore;
use gw_types::packed::{L2Block, Transaction};
use gw_types::{
    packed::{SUDTArgs, SUDTArgsUnion, Script},
    prelude::*,
};
use rust_decimal::Decimal;
use sqlx::types::chrono::{DateTime, NaiveDateTime, Utc};
use sqlx::PgPool;

pub async fn insert_to_sql(
    pool: &PgPool,
    store: Store,
    l1_transaction: &Transaction,
    l2_sudt_type_script_hash: H256,
    polyjuice_type_script_hash: H256,
) -> anyhow::Result<()> {
    let l2_block = extract_l2_block(l1_transaction)?;
    let number: u64 = l2_block.raw().number().unpack();
    let row: Option<(Decimal,)> =
        sqlx::query_as("SELECT number FROM blocks ORDER BY number DESC LIMIT 1")
            .fetch_optional(pool)
            .await?;
    if row.is_none() || Decimal::from(number) == row.unwrap().0 + Decimal::from(1) {
        let web3_tx_with_logs_vec = filter_web3_transactions(
            store.clone(),
            l2_block.clone(),
            l2_sudt_type_script_hash,
            polyjuice_type_script_hash,
        )
        .await?;
        let web3_block = build_web3_block(&pool, &l2_block, &web3_tx_with_logs_vec).await?;
        let mut tx = pool.begin().await?;
        sqlx::query("INSERT INTO blocks (number, hash, parent_hash, logs_bloom, gas_limit, gas_used, timestamp, miner, size) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)")
            .bind(web3_block.number)
            .bind(web3_block.hash)
            .bind(web3_block.parent_hash)
            .bind(web3_block.logs_bloom)
            .bind(web3_block.gas_limit)
            .bind(web3_block.gas_used)
            .bind(web3_block.timestamp)
            .bind(web3_block.miner)
            .bind(web3_block.size)
            .execute(&mut tx).await?;
        for web3_tx_with_logs in web3_tx_with_logs_vec {
            let web3_tx = web3_tx_with_logs.tx;
            let  (transaction_id,): (i32,) =
            sqlx::query_as("INSERT INTO transactions
            (hash, block_number, block_hash, transaction_index, from_address, to_address, value, nonce, gas_limit, gas_price, input, v, r, s, cumulative_gas_used, gas_used, logs_bloom, contract_address, status) 
            VALUES 
            ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19) RETURNING ID")
            .bind(web3_tx.hash)
            .bind(web3_tx.block_number)
            .bind(web3_tx.block_hash)
            .bind(web3_tx.transaction_index)
            .bind(web3_tx.from_address)
            .bind(web3_tx.to_address)
            .bind(web3_tx.value)
            .bind(web3_tx.nonce)
            .bind(web3_tx.gas_limit)
            .bind(web3_tx.gas_price)
            .bind(web3_tx.input)
            .bind(web3_tx.v)
            .bind(web3_tx.r)
            .bind(web3_tx.s)
            .bind(web3_tx.cumulative_gas_used)
            .bind(web3_tx.gas_used)
            .bind(web3_tx.logs_bloom)
            .bind(web3_tx.contract_address)
            .bind(web3_tx.status)
            .fetch_one(&mut tx)
            .await?;

            let web3_logs = web3_tx_with_logs.logs;
            for log in web3_logs {
                sqlx::query("INSERT INTO logs
                (transaction_id, transaction_hash, transaction_index, block_number, block_hash, address, data, log_index, topics)
                VALUES
                ($1, $2, $3, $4, $5, $6, $7, $8, $9)")
                .bind(transaction_id)
                .bind(log.transaction_hash)
                .bind(log.transaction_index)
                .bind(log.block_number)
                .bind(log.block_hash)
                .bind(log.address)
                .bind(log.data)
                .bind(log.log_index)
                .bind(log.topics)
                .execute(&mut tx)
                .await?;
            }
        }
        tx.commit().await?;
    }
    Ok(())
}

fn extract_l2_block(l1_transaction: &Transaction) -> anyhow::Result<L2Block> {
    let witness = l1_transaction
        .witnesses()
        .get(0)
        .ok_or_else(|| anyhow::anyhow!("Witness missing for L2 block!"))?;
    let witness_args = WitnessArgs::from_slice(&witness.raw_data())?;
    let l2_block_bytes = witness_args
        .output_type()
        .to_opt()
        .ok_or_else(|| anyhow::anyhow!("Missing L2 block!"))?;
    let l2_block = L2Block::from_slice(&l2_block_bytes.raw_data())?;
    Ok(l2_block)
}

async fn filter_web3_transactions(
    store: Store,
    l2_block: L2Block,
    l2_sudt_type_script_hash: H256,
    polyjuice_type_script_hash: H256,
) -> anyhow::Result<Vec<Web3TransactionWithLogs>> {
    let block_number = l2_block.raw().number().unpack();
    let block_hash: H256 = blake2b_256(l2_block.raw().as_slice()).into();
    let block_hash_hex = format!("0x{:#x}", block_hash);
    let mut cumulative_gas_used = Decimal::from(0u64);
    let l2_transactions = l2_block.transactions();
    let mut web3_tx_with_logs_vec: Vec<Web3TransactionWithLogs> = vec![];
    let mut tx_index = 0i32;
    for l2_transaction in l2_transactions {
        let tx_hash: H256 = blake2b_256(l2_transaction.raw().as_slice()).into();
        let tx_hash_hex = format!("0x{:#x}", tx_hash);
        println!("tx_hash: {}", tx_hash);
        // extract from_id correspoding script, from_address is the script's args
        let from_id: u32 = l2_transaction.raw().from_id().unpack();
        let from_address = {
            let from_script_hash = get_script_hash(store.clone(), from_id).await?;
            let from_script = get_script(store.clone(), from_script_hash).await?.unwrap();
            from_script.args().raw_data()
        };
        println!("Check from_address: {:#x}", from_address);

        // extract to_id corresponding script, check code_hash is either polyjuice contract code_hash or sudt contract code_hash
        let to_id = l2_transaction.raw().to_id().unpack();
        let to_script_hash = get_script_hash(store.clone(), to_id).await?;
        let to_script = match get_script(store.clone(), to_script_hash).await? {
            Some(s) => s,
            None => continue,
        };

        let mut tx_gas_used = Decimal::from(0u64);
        if to_script.code_hash().as_slice() == polyjuice_type_script_hash.0 {
            let l2_tx_args = l2_transaction.raw().args();
            let polyjuice_args = PolyjuiceArgs::decode(l2_tx_args.raw_data().as_ref())?;
            // to_address is null if it's a contract deployment transaction
            let to_address = if polyjuice_args.is_create {
                None
            } else {
                let address = account_id_to_eth_address(to_id);
                let address_str = faster_hex::hex_string(&address[..])?;
                let address_hex = format!("0x{}", address_str);
                Some(address_hex)
            };
            println!("Check to_address: {:?}", to_address);
            let nonce = {
                let nonce: u32 = l2_transaction.raw().nonce().unpack();
                Decimal::from(nonce)
            };
            println!("Check nonce: {}", nonce);
            let input = match polyjuice_args.input {
                Some(input) => {
                    let input_str = faster_hex::hex_string(&input[..])?;
                    let input_hex = format!("0x{}", input_str);
                    Some(input_hex)
                }
                None => None,
            };
            println!("Check input: {:?}", input);

            let signature: [u8; 65] = l2_transaction.signature().unpack();
            let r = format!("0x{}", faster_hex::hex_string(&signature[0..31])?);
            let s = format!("0x{}", faster_hex::hex_string(&signature[32..63])?);
            let v = format!("0x{}", faster_hex::hex_string(&[signature[64]])?);
            let mut contract_address_hex = None;

            let web3_logs = {
                let db = store.begin_transaction();
                let tx_hash = gw_common::H256::from(tx_hash.0);
                let tx_receipt = db.get_transaction_receipt(&tx_hash)?;
                let mut logs: Vec<Web3Log> = vec![];
                match tx_receipt {
                    Some(tx_receipt) => {
                        let log_item_vec = tx_receipt.logs();
                        let mut log_index = 0;
                        for log_item in log_item_vec {
                            let log = parse_log(&log_item);
                            match log {
                                GwLog::PolyjuiceSystem {
                                    gas_used,
                                    cumulative_gas_used: _,
                                    created_id,
                                    status_code: _,
                                } => {
                                    tx_gas_used = Decimal::from(gas_used);
                                    cumulative_gas_used += tx_gas_used;
                                    if polyjuice_args.is_create && created_id != u32::MAX {
                                        let contract_address =
                                            account_id_to_eth_address(created_id);
                                        contract_address_hex = Some(format!(
                                            "0x{}",
                                            faster_hex::hex_string(&contract_address[..])?
                                        ));
                                    }
                                }
                                GwLog::PolyjuiceUser {
                                    address,
                                    data,
                                    topics,
                                } => {
                                    let address =
                                        format!("0x{}", faster_hex::hex_string(&address[..])?);
                                    let data = format!("0x{}", faster_hex::hex_string(&data[..])?);
                                    let mut topics_hex = vec![];
                                    for topic in topics {
                                        let topic_hex = format!(
                                            "0x{}",
                                            faster_hex::hex_string(topic.as_slice())?
                                        );
                                        topics_hex.push(topic_hex);
                                    }

                                    let web3_log = Web3Log::new(
                                        tx_hash_hex.clone(),
                                        tx_index,
                                        Decimal::from(block_number),
                                        block_hash_hex.clone(),
                                        address,
                                        data,
                                        log_index,
                                        topics_hex,
                                    );
                                    logs.push(web3_log);
                                    log_index += 1;
                                }
                                GwLog::SudtTransfer {
                                    sudt_id: _,
                                    from_id: _,
                                    to_id: _,
                                    amount: _,
                                } => {
                                    // TODO: SudtTransfer happened in polyjuice contract will be include in web3 events later.
                                }
                            }
                        }
                    }
                    None => {}
                }
                logs
            };

            let web3_transaction = Web3Transaction::new(
                tx_hash_hex.clone(),
                Decimal::from(block_number),
                block_hash_hex.clone(),
                tx_index as i32,
                format!("{:#x}", from_address),
                to_address,
                Decimal::from(polyjuice_args.value),
                nonce,
                Decimal::from(polyjuice_args.gas_limit),
                Decimal::from(polyjuice_args.gas_price),
                input,
                r,
                s,
                v,
                cumulative_gas_used,
                tx_gas_used,
                String::from("0x"),
                contract_address_hex,
                true,
            );

            println!("web3 transaction: {:?}", web3_transaction);
            let web3_tx_with_logs = Web3TransactionWithLogs {
                tx: web3_transaction,
                logs: web3_logs,
            };
            web3_tx_with_logs_vec.push(web3_tx_with_logs);
            tx_index += 1;
        } else if to_id == CKB_SUDT_ACCOUNT_ID
            && to_script.code_hash().as_slice() == l2_sudt_type_script_hash.0
        {
            // deal with SUDT transfer
            let sudt_args =
                match SUDTArgs::from_slice(l2_transaction.raw().args().raw_data().as_ref()) {
                    Ok(s) => s,
                    Err(e) => {
                        println!("SUDArgs error: {:?}", e);
                        continue;
                    }
                };
            match sudt_args.to_enum() {
                SUDTArgsUnion::SUDTTransfer(sudt_transfer) => {
                    let to_id: u32 = sudt_transfer.to().unpack();
                    let amount: u128 = sudt_transfer.amount().unpack();
                    let fee: u128 = sudt_transfer.fee().unpack();

                    let to_script_hash = get_script_hash(store.clone(), to_id).await?;
                    let to_script = match get_script(store.clone(), to_script_hash).await? {
                        Some(s) => s,
                        None => continue,
                    };

                    let to_address = format!("{:#x}", to_script.args().raw_data());
                    let value = amount;

                    // Represent SUDTTransfer fee in web3 style, set gas_price as 1 temporary.
                    let gas_price = Decimal::from(1);
                    let gas_limit = Decimal::from(fee);
                    cumulative_gas_used += gas_limit;

                    let nonce = {
                        let nonce: u32 = l2_transaction.raw().nonce().unpack();
                        Decimal::from(nonce)
                    };

                    let signature: [u8; 65] = l2_transaction.signature().unpack();
                    let r = format!("0x{}", faster_hex::hex_string(&signature[0..31])?);
                    let s = format!("0x{}", faster_hex::hex_string(&signature[32..63])?);
                    let v = format!("0x{}", faster_hex::hex_string(&[signature[64]])?);

                    let web3_transaction = Web3Transaction::new(
                        tx_hash_hex.clone(),
                        Decimal::from(block_number),
                        block_hash_hex.clone(),
                        tx_index as i32,
                        format!("{:#x}", from_address),
                        Some(to_address),
                        Decimal::from(value),
                        nonce,
                        gas_limit,
                        gas_price,
                        None,
                        r,
                        s,
                        v,
                        cumulative_gas_used,
                        cumulative_gas_used,
                        String::from("0x"),
                        None,
                        true,
                    );

                    println!("web3 transaction: {:?}", web3_transaction);
                    let web3_tx_with_logs = Web3TransactionWithLogs {
                        tx: web3_transaction,
                        logs: vec![],
                    };
                    web3_tx_with_logs_vec.push(web3_tx_with_logs);
                }
                SUDTArgsUnion::SUDTQuery(_sudt_query) => {}
            }
            tx_index += 1;
        }
    }
    Ok(web3_tx_with_logs_vec)
}

async fn build_web3_block(
    pool: &PgPool,
    l2_block: &L2Block,
    web3_tx_with_logs_vec: &Vec<Web3TransactionWithLogs>,
) -> anyhow::Result<Web3Block> {
    let block_number = l2_block.raw().number().unpack();
    let block_hash: H256 = blake2b_256(l2_block.raw().as_slice()).into();
    let parent_hash = {
        if block_number == 0 {
            String::from("0x0000000000000000000000000000000000000000000000000000000000000000")
        } else {
            let row: Option<(String,)> =
                sqlx::query_as("SELECT hash FROM blocks WHERE number = $1")
                    .bind(Decimal::from(block_number - 1))
                    .fetch_optional(pool)
                    .await?;
            match row {
                Some(block) => block.0,
                None => panic!("No parent hash found!"),
            }
        }
    };
    let mut gas_limit = Decimal::from(0);
    let mut gas_used = Decimal::from(0);
    for web3_tx_with_logs in web3_tx_with_logs_vec {
        gas_limit += web3_tx_with_logs.tx.gas_limit;
        gas_used += web3_tx_with_logs.tx.gas_used;
    }
    let epoch_time: u64 = l2_block.raw().timestamp().unpack();
    let web3_block = Web3Block {
        number: Decimal::from(block_number),
        hash: format!("{:#x}", block_hash),
        parent_hash: parent_hash,
        logs_bloom: String::from(""),
        gas_limit: gas_limit,
        gas_used: gas_used,
        miner: format!("{}", l2_block.raw().block_producer_id()),
        size: Decimal::from(0),
        timestamp: DateTime::<Utc>::from_utc(
            NaiveDateTime::from_timestamp(epoch_time as i64, 0),
            Utc,
        ),
    };
    Ok(web3_block)
}

// async fn insert_to_block(sql_tx: & , block: Web3Block) {
// }

// async fn insert_to_transaction(tx: Web3Transaction) {
// }

// async fn insert_to_log(log: Web3Log) {}

async fn get_script_hash(store: Store, account_id: u32) -> anyhow::Result<gw_common::H256> {
    let db = store.begin_transaction();
    let tip_hash = db.get_tip_block_hash()?;
    let state_db =
        StateDBTransaction::from_version(&db, StateDBVersion::from_block_hash(tip_hash))?;
    let tree = state_db.account_state_tree()?;

    let script_hash = tree.get_script_hash(account_id)?;
    Ok(script_hash)
}

async fn get_script(store: Store, script_hash: gw_common::H256) -> anyhow::Result<Option<Script>> {
    let db = store.begin_transaction();
    let tip_hash = db.get_tip_block_hash()?;
    let state_db =
        StateDBTransaction::from_version(&db, StateDBVersion::from_block_hash(tip_hash))?;
    let tree = state_db.account_state_tree()?;

    let script_opt = tree.get_script(&script_hash);
    Ok(script_opt)
}