use crate::{
    block_producer::BlockProducer, challenger::Challenger, cleaner::Cleaner, poa::PoA,
    poller::ChainUpdater, rpc_client::RPCClient, test_mode_control::TestModeControl,
    types::ChainEvent, utils::CKBGenesisInfo, wallet::Wallet,
};
use anyhow::{anyhow, Context, Result};
use async_jsonrpc_client::HttpClient;
use futures::{select, FutureExt};
use gw_chain::chain::Chain;
use gw_common::H256;
use gw_config::{BlockProducerConfig, Config, NodeMode};
use gw_db::{config::Config as DBConfig, schema::COLUMNS, RocksDB};
use gw_generator::{
    account_lock_manage::{secp256k1::Secp256k1Eth, AccountLockManage},
    backend_manage::BackendManage,
    genesis::init_genesis,
    Generator, RollupContext,
};
use gw_mem_pool::pool::MemPool;
use gw_rpc_server::{registry::Registry, server::start_jsonrpc_server};
use gw_store::Store;
use gw_types::prelude::{Pack, Unpack};
use gw_types::{
    bytes::Bytes,
    packed::{NumberHash, RollupConfig, Script},
    prelude::*,
};
use gw_web3_indexer::Web3Indexer;
use parking_lot::Mutex;
use semver::Version;
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    ConnectOptions,
};
use std::{
    net::{SocketAddr, ToSocketAddrs},
    process::exit,
    sync::Arc,
    time::Duration,
};

const MIN_CKB_VERSION: &str = "0.40.0";

async fn poll_loop(
    rpc_client: RPCClient,
    chain_updater: ChainUpdater,
    block_producer: Option<BlockProducer>,
    challenger: Option<Challenger>,
    cleaner: Option<Arc<Cleaner>>,
    poll_interval: Duration,
) -> Result<()> {
    struct Inner {
        chain_updater: ChainUpdater,
        block_producer: Option<BlockProducer>,
        challenger: Option<Challenger>,
        cleaner: Option<Arc<Cleaner>>,
    }

    let inner = Arc::new(smol::lock::Mutex::new(Inner {
        chain_updater,
        challenger,
        block_producer,
        cleaner,
    }));
    // get tip
    let (mut tip_number, mut tip_hash) = {
        let tip = rpc_client.get_tip().await?;
        let tip_number: u64 = tip.number().unpack();
        let tip_hash: H256 = tip.block_hash().unpack();
        (tip_number, tip_hash)
    };
    loop {
        if let Some(block) = rpc_client.get_block_by_number(tip_number + 1).await? {
            let raw_header = block.header().raw();
            let event = if raw_header.parent_hash().as_slice() == tip_hash.as_slice() {
                // received new layer1 block
                log::info!(
                    "received new layer1 block {}, {}",
                    tip_number,
                    hex::encode(tip_hash.as_slice())
                );
                ChainEvent::NewBlock {
                    block: block.clone(),
                }
            } else {
                // layer1 reverted
                log::info!(
                    "layer1 reverted {}, {:?}",
                    tip_number,
                    hex::encode(tip_hash.as_slice())
                );
                ChainEvent::Reverted {
                    old_tip: NumberHash::new_builder()
                        .number(tip_number.pack())
                        .block_hash(tip_hash.pack())
                        .build(),
                    new_block: block.clone(),
                }
            };
            // must execute chain update before block producer, otherwise we may run into an invalid chain state
            // smol::spawn({
            let event = event.clone();
            let inner = inner.clone();
            // async move {
            let mut inner = inner.lock().await;
            if let Err(err) = inner.chain_updater.handle_event(event.clone()).await {
                log::error!(
                    "Error occured when polling chain_updater, event: {:?}, error: {}",
                    event,
                    err
                );
            }

            if let Some(ref mut challenger) = inner.challenger {
                if let Err(err) = challenger.handle_event(event.clone()).await {
                    log::error!(
                        "Error occured when polling challenger, event: {:?}, error: {}",
                        event,
                        err
                    );
                }
            }

            if let Some(ref mut block_producer) = inner.block_producer {
                if let Err(err) = block_producer.handle_event(event.clone()).await {
                    log::error!(
                        "Error occured when polling block_producer, event: {:?}, error: {}",
                        event,
                        err
                    );
                }
            }

            if let Some(ref cleaner) = inner.cleaner {
                if let Err(err) = cleaner.handle_event(event.clone()).await {
                    log::error!(
                        "Error occured when polling block_producer, event: {:?}, error: {}",
                        event,
                        err
                    );
                }
            }

            // }
            // })
            // .detach();
            // update tip
            tip_number = raw_header.number().unpack();
            tip_hash = block.header().hash().into();
        } else {
            log::debug!(
                "Not found layer1 block #{} sleep {}s then retry",
                tip_number + 1,
                poll_interval.as_secs()
            );
            async_std::task::sleep(poll_interval).await;
        }
    }
}

pub fn run(config: Config, skip_config_check: bool) -> Result<()> {
    let rollup_config: RollupConfig = config.genesis.rollup_config.clone().into();
    let rollup_context = RollupContext {
        rollup_config: rollup_config.clone(),
        rollup_script_hash: {
            let rollup_script_hash: [u8; 32] = config.genesis.rollup_type_hash.clone().into();
            rollup_script_hash.into()
        },
    };
    let rollup_type_script: Script = config.chain.rollup_type_script.clone().into();
    let rpc_client = {
        let indexer_client = HttpClient::new(config.rpc_client.indexer_url)?;
        let ckb_client = HttpClient::new(config.rpc_client.ckb_url)?;
        let rollup_type_script =
            ckb_types::packed::Script::new_unchecked(rollup_type_script.as_bytes());
        RPCClient {
            indexer_client,
            ckb_client,
            rollup_context: rollup_context.clone(),
            rollup_type_script,
        }
    };

    if !skip_config_check {
        check_ckb_version(&rpc_client)?;
        // TODO: check ckb indexer version
        if NodeMode::ReadOnly != config.node_mode {
            let block_producer_config = config
                .block_producer
                .clone()
                .ok_or_else(|| anyhow!("not set block producer"))?;
            check_rollup_config_cell(&block_producer_config, &rollup_config, &rpc_client)?;
            check_locks(&block_producer_config, &rollup_config)?;
        }
    }

    // Open store
    let store = if config.store.path.as_os_str().is_empty() {
        log::warn!("config.store.path is blank, using temporary store");
        Store::open_tmp().with_context(|| "init store")?
    } else {
        let db_config = DBConfig {
            path: config.store.path,
            options: Default::default(),
            options_file: Default::default(),
        };
        Store::new(RocksDB::open(&db_config, COLUMNS))
    };
    let secp_data: Bytes = {
        let out_point = config.genesis.secp_data_dep.out_point.clone();
        smol::block_on(rpc_client.get_transaction(out_point.tx_hash.0.into()))?
            .ok_or_else(|| anyhow!("can not found transaction: {:?}", out_point.tx_hash))?
            .raw()
            .outputs_data()
            .get(out_point.index.value() as usize)
            .expect("get secp output data")
            .raw_data()
    };
    init_genesis(
        &store,
        &config.genesis,
        config.chain.genesis_committed_info.clone().into(),
        secp_data,
    )
    .with_context(|| "init genesis")?;

    let rollup_config_hash: H256 = rollup_config.hash().into();
    let generator = {
        let backend_manage = BackendManage::from_config(config.backends.clone())
            .with_context(|| "config backends")?;
        let mut account_lock_manage = AccountLockManage::default();
        let eth_lock_script_type_hash = rollup_config
            .allowed_eoa_type_hashes()
            .get(0)
            .ok_or_else(|| anyhow!("No allowed EoA type hashes in the rollup config"))?;
        account_lock_manage.register_lock_algorithm(
            eth_lock_script_type_hash.unpack(),
            Box::new(Secp256k1Eth::default()),
        );
        Arc::new(Generator::new(
            backend_manage,
            account_lock_manage,
            rollup_context.clone(),
        ))
    };
    let mem_pool = Arc::new(Mutex::new(
        MemPool::create(store.clone(), generator.clone()).with_context(|| "create mem-pool")?,
    ));
    let chain = Arc::new(Mutex::new(
        Chain::create(
            &rollup_config,
            &config.chain.rollup_type_script.clone().into(),
            store.clone(),
            generator.clone(),
            mem_pool.clone(),
        )
        .with_context(|| "create chain")?,
    ));

    // create web3 indexer
    let web3_indexer = match config.web3_indexer {
        Some(web3_indexer_config) => {
            let pool = smol::block_on(async {
                let mut opts: PgConnectOptions = web3_indexer_config.database_url.parse()?;
                opts.log_statements(log::LevelFilter::Debug)
                    .log_slow_statements(log::LevelFilter::Warn, Duration::from_secs(5));
                PgPoolOptions::new()
                    .max_connections(5)
                    .connect_with(opts)
                    .await
            })?;
            let polyjuce_type_script_hash = web3_indexer_config.polyjuice_script_type_hash;
            let eth_account_lock_hash = web3_indexer_config.eth_account_lock_hash;
            let web3_indexer = Web3Indexer::new(
                pool,
                config
                    .genesis
                    .rollup_config
                    .l2_sudt_validator_script_type_hash,
                polyjuce_type_script_hash,
                config.genesis.rollup_type_hash,
                eth_account_lock_hash,
            );
            Some(web3_indexer)
        }
        None => None,
    };

    // create chain updater
    let chain_updater = ChainUpdater::new(
        Arc::clone(&chain),
        rpc_client.clone(),
        rollup_context.clone(),
        rollup_type_script.clone(),
        web3_indexer,
    );

    let ckb_genesis_info = {
        let ckb_genesis = smol::block_on(async { rpc_client.get_block_by_number(0).await })?
            .ok_or_else(|| anyhow!("can't found CKB genesis block"))?;
        CKBGenesisInfo::from_block(&ckb_genesis)?
    };

    let (block_producer, challenger, test_mode_control, cleaner) = match config.node_mode {
        NodeMode::ReadOnly => (None, None, None, None),
        _ => {
            let block_producer_config = config
                .block_producer
                .clone()
                .ok_or_else(|| anyhow!("not set block producer"))?;

            let wallet = Wallet::from_config(&block_producer_config.wallet_config)
                .with_context(|| "init wallet")?;

            let poa = {
                let poa = PoA::new(
                    rpc_client.clone(),
                    wallet.lock_script().to_owned(),
                    block_producer_config.poa_lock_dep.clone().into(),
                    block_producer_config.poa_state_dep.clone().into(),
                );
                Arc::new(smol::lock::Mutex::new(poa))
            };

            let tests_control = if let NodeMode::Test = config.node_mode {
                Some(TestModeControl::new(
                    rpc_client.clone(),
                    Arc::clone(&poa),
                    store.clone(),
                ))
            } else {
                None
            };

            // Block Producer
            let block_producer = BlockProducer::create(
                rollup_config_hash,
                store.clone(),
                generator.clone(),
                Arc::clone(&chain),
                mem_pool.clone(),
                rpc_client.clone(),
                ckb_genesis_info.clone(),
                block_producer_config.clone(),
                tests_control.clone(),
            )
            .with_context(|| "init block producer")?;

            let cleaner = Arc::new(Cleaner::new(
                rpc_client.clone(),
                ckb_genesis_info.clone(),
                wallet,
            ));

            let wallet = Wallet::from_config(&block_producer_config.wallet_config)
                .with_context(|| "init wallet")?;

            // Challenger
            let challenger = Challenger::new(
                rollup_context,
                rpc_client.clone(),
                wallet,
                block_producer_config,
                ckb_genesis_info,
                Arc::clone(&chain),
                Arc::clone(&poa),
                tests_control.clone(),
                Arc::clone(&cleaner),
            );

            (
                Some(block_producer),
                Some(challenger),
                tests_control,
                Some(cleaner),
            )
        }
    };

    // RPC registry
    let rpc_registry = Registry::new(store, mem_pool, generator, test_mode_control.map(Box::new));

    let (s, ctrl_c) = async_channel::bounded(100);
    let handle = move || {
        s.try_send(()).ok();
    };
    ctrlc::set_handler(handle).unwrap();

    let rpc_address: SocketAddr = {
        let mut addrs: Vec<_> = config.rpc_server.listen.to_socket_addrs()?.collect();
        if addrs.len() != 1 {
            return Err(anyhow!(
                "Invalid RPC listen address `{}`",
                &config.rpc_server.listen
            ));
        }
        addrs.remove(0)
    };

    {
        let rollup_type_script_hash = {
            let hash = rollup_type_script.hash();
            ckb_fixed_hash::H256::from_slice(&hash).unwrap()
        };
        let rollup_config_hash =
            ckb_fixed_hash::H256::from_slice(&rollup_config_hash.as_slice()).unwrap();
        log::info!("Rollup type script hash: {}", rollup_type_script_hash);
        log::info!("Rollup config hash: {}", rollup_config_hash);
    }

    log::info!("{:?} mode enabled!!!", config.node_mode);

    smol::block_on(async {
        select! {
            _ = ctrl_c.recv().fuse() => log::info!("Exiting..."),
            e = poll_loop(rpc_client, chain_updater, block_producer, challenger, cleaner, Duration::from_secs(3)).fuse() => {
                log::error!("Error in main poll loop: {:?}", e);
            }
            e = start_jsonrpc_server(rpc_address, rpc_registry).fuse() => {
                log::error!("Error running JSONRPC server: {:?}", e);
                exit(1);
            },
        };
    });

    Ok(())
}

fn check_ckb_version(rpc_client: &RPCClient) -> Result<()> {
    let ckb_version = smol::block_on(rpc_client.get_ckb_version())?;
    let ckb_version = ckb_version.split('(').collect::<Vec<&str>>()[0].trim_end();
    if Version::parse(&ckb_version)? < Version::parse(MIN_CKB_VERSION)? {
        return Err(anyhow!(
            "The version of CKB node {} is lower than the minimum version {}",
            ckb_version,
            MIN_CKB_VERSION
        ));
    }
    Ok(())
}

fn check_rollup_config_cell(
    block_producer_config: &BlockProducerConfig,
    rollup_config: &RollupConfig,
    rpc_client: &RPCClient,
) -> Result<()> {
    let rollup_config_cell = smol::block_on(
        rpc_client.get_cell(
            block_producer_config
                .rollup_config_cell_dep
                .out_point
                .clone()
                .into(),
        ),
    )?
    .ok_or_else(|| anyhow!("can't find rollup config cell"))?;
    let cell_data = RollupConfig::from_slice(&rollup_config_cell.data.to_vec())?;
    let eoa_set = rollup_config
        .allowed_eoa_type_hashes()
        .into_iter()
        .collect::<Vec<_>>();
    let contract_set = rollup_config
        .allowed_contract_type_hashes()
        .into_iter()
        .collect::<Vec<_>>();
    let unregistered_eoas = cell_data
        .allowed_eoa_type_hashes()
        .into_iter()
        .filter(|item| !eoa_set.contains(&item))
        .collect::<Vec<_>>();
    let unregistered_contracts = cell_data
        .allowed_contract_type_hashes()
        .into_iter()
        .filter(|item| !contract_set.contains(&item))
        .collect::<Vec<_>>();
    if !unregistered_eoas.is_empty() || !unregistered_contracts.is_empty() {
        return Err(anyhow!(
            "Unregistered eoa type hashes: {:#?}, \
            unregistered contract type hashes: {:#?}",
            unregistered_eoas,
            unregistered_contracts
        ));
    }
    Ok(())
}

fn check_locks(
    block_producer_config: &BlockProducerConfig,
    rollup_config: &RollupConfig,
) -> Result<()> {
    let zeros = ckb_fixed_hash::H256([0u8; 32]);

    // check burn lock
    if zeros != block_producer_config.challenger_config.burn_lock.code_hash {
        return Err(anyhow!(
            "[block_producer.challenger.burn_lock.code_hash] is expected to be zero"
        ));
    }

    let burn_lock_hash = {
        let script: gw_types::packed::Script = block_producer_config
            .challenger_config
            .burn_lock
            .clone()
            .into();
        script.hash().pack()
    };
    if burn_lock_hash != rollup_config.burn_lock_hash() {
        return Err(anyhow!("[block_producer.challenge.burn_lock] ({}) isn't match rollup config's burn_lock_hash ({})", burn_lock_hash, rollup_config.burn_lock_hash()));
    }

    // check challenge lock
    if zeros
        == block_producer_config
            .challenger_config
            .rewards_receiver_lock
            .code_hash
    {
        return Err(anyhow!(
            "[block_producer.challenger.rewards_receiver_lock.code_hash] shouldn't be zero"
        ));
    }

    // check wallet lock
    if zeros == block_producer_config.wallet_config.lock.code_hash {
        return Err(anyhow!(
            "[block_producer.wallet.lock.code_hash] shouldn't be zero"
        ));
    }
    if block_producer_config.wallet_config.lock
        == block_producer_config
            .challenger_config
            .rewards_receiver_lock
    {
        return Err(anyhow!(
            "[block_producer.challenger.rewards_receiver_lock] and [block_producer.wallet.lock] have the same address, which is not recommended"
        ));
    }
    Ok(())
}
