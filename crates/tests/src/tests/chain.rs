use crate::testing_tool::chain::{
    build_sync_tx, construct_block, setup_chain, ALWAYS_SUCCESS_CODE_HASH,
};
use gw_chain::chain::{
    Chain, L1Action, L1ActionContext, RevertL1ActionContext, RevertedL1Action, SyncParam,
};
use gw_common::{
    builtins::CKB_SUDT_ACCOUNT_ID,
    state::{to_short_address, State},
    H256,
};
use gw_store::state_db::{CheckPoint, StateDBMode, StateDBTransaction, SubState};
use gw_types::{
    core::ScriptHashType,
    packed::{CellOutput, DepositRequest, GlobalState, L2BlockCommittedInfo, Script},
    prelude::*,
};

fn produce_a_block(
    chain: &mut Chain,
    deposit: DepositRequest,
    rollup_cell: CellOutput,
    expected_tip: u64,
) -> SyncParam {
    let block_result = {
        let mem_pool = chain.mem_pool().lock();
        construct_block(&chain, &mem_pool, vec![deposit.clone()]).unwrap()
    };
    let l2block = block_result.block.clone();
    let transaction = build_sync_tx(rollup_cell, block_result);
    let l2block_committed_info = L2BlockCommittedInfo::new_builder()
        .number(expected_tip.pack())
        .build();

    let update = L1Action {
        context: L1ActionContext::SubmitBlock {
            l2block,
            deposit_requests: vec![deposit.clone()],
        },
        transaction,
        l2block_committed_info,
    };
    let param = SyncParam {
        updates: vec![update],
        reverts: Default::default(),
    };
    chain.sync(param.clone()).unwrap();
    assert_eq!(chain.last_sync_event().is_success(), true);

    assert_eq!(
        {
            let tip_block_number: u64 = chain
                .store()
                .get_tip_block()
                .unwrap()
                .raw()
                .number()
                .unpack();
            tip_block_number
        },
        expected_tip
    );
    param
}

#[test]
fn test_produce_blocks() {
    let rollup_type_script = Script::default();
    let rollup_script_hash = rollup_type_script.hash();
    let mut chain = setup_chain(rollup_type_script.clone());

    let rollup_cell = CellOutput::new_builder()
        .type_(Some(rollup_type_script).pack())
        .build();

    // block #1
    let user_script_a = Script::new_builder()
        .code_hash(ALWAYS_SUCCESS_CODE_HASH.clone().pack())
        .hash_type(ScriptHashType::Type.into())
        .args({
            let mut args = rollup_script_hash.to_vec();
            args.push(42);
            args.pack()
        })
        .build();
    let deposit = DepositRequest::new_builder()
        .capacity(100u64.pack())
        .script(user_script_a.clone())
        .build();
    produce_a_block(&mut chain, deposit, rollup_cell.clone(), 1);

    // block #2
    let deposit = DepositRequest::new_builder()
        .capacity(200u64.pack())
        .script(user_script_a.clone())
        .build();
    produce_a_block(&mut chain, deposit, rollup_cell.clone(), 2);

    // block #3
    let user_script_b = Script::new_builder()
        .code_hash(ALWAYS_SUCCESS_CODE_HASH.clone().pack())
        .hash_type(ScriptHashType::Type.into())
        .args({
            let mut args = rollup_script_hash.to_vec();
            args.push(50);
            args.pack()
        })
        .build();
    let deposit = DepositRequest::new_builder()
        .capacity(500u64.pack())
        .script(user_script_b.clone())
        .build();
    produce_a_block(&mut chain, deposit, rollup_cell.clone(), 3);

    // check state
    {
        let db = chain.store().begin_transaction();
        let tip_block_hash = db.get_tip_block_hash().unwrap();
        let state_db = StateDBTransaction::from_checkpoint(
            &db,
            CheckPoint::from_block_hash(&db, tip_block_hash, SubState::Block).unwrap(),
            StateDBMode::ReadOnly,
        )
        .unwrap();
        let tree = state_db.account_state_tree().unwrap();
        let script_hash_a: H256 = user_script_a.hash().into();
        let script_hash_b: H256 = user_script_b.hash().into();
        let id_a = tree
            .get_account_id_by_script_hash(&script_hash_a)
            .unwrap()
            .unwrap();
        let id_b = tree
            .get_account_id_by_script_hash(&script_hash_b)
            .unwrap()
            .unwrap();
        // 0 is meta contract, 1 is ckb sudt, so the user id start from 2
        assert_eq!(id_a, 2);
        assert_eq!(id_b, 3);
        let balance_a = tree
            .get_sudt_balance(CKB_SUDT_ACCOUNT_ID, to_short_address(&script_hash_a))
            .unwrap();
        let balance_b = tree
            .get_sudt_balance(CKB_SUDT_ACCOUNT_ID, to_short_address(&script_hash_b))
            .unwrap();
        assert_eq!(balance_a, 300);
        assert_eq!(balance_b, 500);
    }

    drop(chain);
}

#[test]
fn test_layer1_fork() {
    let rollup_type_script = Script::default();
    let rollup_script_hash = rollup_type_script.hash();
    let mut chain = setup_chain(rollup_type_script.clone());
    let rollup_cell = CellOutput::new_builder()
        .type_(Some(rollup_type_script.clone()).pack())
        .build();

    // build fork block 1
    let fork_action = {
        // build fork from another chain to avoid mess up the tx pool
        let charlie_script = Script::new_builder()
            .code_hash(ALWAYS_SUCCESS_CODE_HASH.clone().pack())
            .hash_type(ScriptHashType::Type.into())
            .args({
                let mut args = rollup_script_hash.to_vec();
                args.push(7);
                args.pack()
            })
            .build();
        let deposit = DepositRequest::new_builder()
            .capacity(120u64.pack())
            .script(charlie_script)
            .build();
        let chain = setup_chain(rollup_type_script.clone());
        let mem_pool = chain.mem_pool().lock();
        let block_result = construct_block(&chain, &mem_pool, vec![deposit.clone()]).unwrap();

        L1Action {
            context: L1ActionContext::SubmitBlock {
                l2block: block_result.block.clone(),
                deposit_requests: vec![deposit],
            },
            transaction: build_sync_tx(rollup_cell.clone(), block_result),
            l2block_committed_info: L2BlockCommittedInfo::new_builder()
                .number(1u64.pack())
                .build(),
        }
    };
    // update block 1
    let alice_script = Script::new_builder()
        .code_hash(ALWAYS_SUCCESS_CODE_HASH.clone().pack())
        .hash_type(ScriptHashType::Type.into())
        .args({
            let mut args = rollup_script_hash.to_vec();
            args.push(42);
            args.pack()
        })
        .build();
    let deposit = DepositRequest::new_builder()
        .capacity(100u64.pack())
        .script(alice_script)
        .build();
    let block_result = {
        let mem_pool = chain.mem_pool().lock();
        construct_block(&chain, &mem_pool, vec![deposit.clone()]).unwrap()
    };
    let action1 = L1Action {
        context: L1ActionContext::SubmitBlock {
            l2block: block_result.block.clone(),
            deposit_requests: vec![deposit.clone()],
        },
        transaction: build_sync_tx(rollup_cell.clone(), block_result),
        l2block_committed_info: L2BlockCommittedInfo::new_builder()
            .number(1u64.pack())
            .build(),
    };
    let param = SyncParam {
        updates: vec![action1.clone()],
        reverts: Default::default(),
    };
    chain.sync(param).unwrap();
    assert_eq!(chain.last_sync_event().is_success(), true);
    // update block 2
    let bob_script = Script::new_builder()
        .code_hash(ALWAYS_SUCCESS_CODE_HASH.clone().pack())
        .hash_type(ScriptHashType::Type.into())
        .args({
            let mut args = rollup_script_hash.to_vec();
            args.push(43);
            args.pack()
        })
        .build();
    let deposit = DepositRequest::new_builder()
        .capacity(500u64.pack())
        .script(bob_script)
        .build();
    let block_result = {
        let mem_pool = chain.mem_pool().lock();
        construct_block(&chain, &mem_pool, vec![deposit.clone()]).unwrap()
    };
    let action2 = L1Action {
        context: L1ActionContext::SubmitBlock {
            l2block: block_result.block.clone(),
            deposit_requests: vec![deposit],
        },
        transaction: build_sync_tx(rollup_cell.clone(), block_result),
        l2block_committed_info: L2BlockCommittedInfo::new_builder()
            .number(2u64.pack())
            .build(),
    };
    let param = SyncParam {
        updates: vec![action2.clone()],
        reverts: Default::default(),
    };
    chain.sync(param).unwrap();
    assert_eq!(chain.last_sync_event().is_success(), true);
    let tip_block = chain.store().get_tip_block().unwrap();
    let tip_block_number: u64 = tip_block.raw().number().unpack();
    assert_eq!(tip_block_number, 2);

    // revert blocks
    let db = chain.store().begin_transaction();
    let tip_block_parent_hash: H256 = tip_block.raw().parent_block_hash().unpack();
    let revert_action2 = {
        let prev_global_state = db
            .get_block_post_global_state(&tip_block_parent_hash)
            .unwrap()
            .unwrap();
        let l2block_committed_info = db
            .get_l2block_committed_info(&tip_block_parent_hash)
            .unwrap()
            .unwrap();
        let context = RevertL1ActionContext::SubmitValidBlock { l2block: tip_block };
        RevertedL1Action {
            prev_global_state,
            l2block_committed_info,
            context,
        }
    };
    let tip_parent_block = db.get_block(&tip_block_parent_hash).unwrap().unwrap();
    let tip_grandpa_block_hash: H256 = tip_parent_block.raw().parent_block_hash().unpack();
    let revert_action1 = {
        let prev_global_state = db
            .get_block_post_global_state(&tip_grandpa_block_hash)
            .unwrap()
            .unwrap();
        let l2block_committed_info = db
            .get_l2block_committed_info(&tip_grandpa_block_hash)
            .unwrap()
            .unwrap();
        let context = RevertL1ActionContext::SubmitValidBlock {
            l2block: tip_parent_block,
        };
        RevertedL1Action {
            prev_global_state,
            l2block_committed_info,
            context,
        }
    };
    let forks = vec![fork_action];

    let param = SyncParam {
        updates: forks,
        reverts: vec![revert_action2, revert_action1],
    };
    chain.sync(param).unwrap();
    assert_eq!(chain.last_sync_event().is_success(), true);

    let tip_block = chain.store().get_tip_block().unwrap();
    let tip_block_number: u64 = tip_block.raw().number().unpack();
    assert_eq!(tip_block_number, 1);

    // check account SMT, should be able to calculate account state root
    {
        let db = chain.store().begin_transaction();
        let db = StateDBTransaction::from_checkpoint(
            &db,
            CheckPoint::from_block_hash(&db, tip_block.hash().into(), SubState::Block).unwrap(),
            StateDBMode::ReadOnly,
        )
        .unwrap();
        let tree = db.account_state_tree().unwrap();
        let current_account_root = tree.calculate_root().unwrap();
        let expected_account_root: H256 = tip_block.raw().post_account().merkle_root().unpack();
        assert_eq!(
            current_account_root, expected_account_root,
            "check account tree"
        );
    }
}

#[test]
fn test_layer1_revert() {
    let rollup_type_script = Script::default();
    let rollup_script_hash = rollup_type_script.hash();
    let mut chain = setup_chain(rollup_type_script.clone());
    let rollup_cell = CellOutput::new_builder()
        .type_(Some(rollup_type_script.clone()).pack())
        .build();

    let default_eoa_code_hash = chain
        .generator()
        .rollup_context()
        .rollup_config
        .allowed_eoa_type_hashes()
        .get(0)
        .expect("get default EoA hash");

    // update block 1
    let alice_script = Script::new_builder()
        .code_hash(default_eoa_code_hash.clone())
        .hash_type(ScriptHashType::Type.into())
        .args({
            let mut args = rollup_script_hash.to_vec();
            args.push(42);
            args.pack()
        })
        .build();
    let deposit = DepositRequest::new_builder()
        .capacity(100u64.pack())
        .script(alice_script.clone())
        .build();
    let block_result = {
        let mem_pool = chain.mem_pool().lock();
        construct_block(&chain, &mem_pool, vec![deposit.clone()]).unwrap()
    };
    let action1 = L1Action {
        context: L1ActionContext::SubmitBlock {
            l2block: block_result.block.clone(),
            deposit_requests: vec![deposit.clone()],
        },
        transaction: build_sync_tx(rollup_cell.clone(), block_result),
        l2block_committed_info: L2BlockCommittedInfo::new_builder()
            .number(1u64.pack())
            .build(),
    };
    let param = SyncParam {
        updates: vec![action1.clone()],
        reverts: Default::default(),
    };
    chain.sync(param).unwrap();
    assert_eq!(chain.last_sync_event().is_success(), true);
    // update block 2
    let bob_script = Script::new_builder()
        .code_hash(default_eoa_code_hash)
        .hash_type(ScriptHashType::Type.into())
        .args({
            let mut args = rollup_script_hash.to_vec();
            args.push(43);
            args.pack()
        })
        .build();
    let deposit = DepositRequest::new_builder()
        .capacity(500u64.pack())
        .script(bob_script.clone())
        .build();
    let block_result = {
        let mem_pool = chain.mem_pool().lock();
        construct_block(&chain, &mem_pool, vec![deposit.clone()]).unwrap()
    };
    let action2 = L1Action {
        context: L1ActionContext::SubmitBlock {
            l2block: block_result.block.clone(),
            deposit_requests: vec![deposit],
        },
        transaction: build_sync_tx(rollup_cell.clone(), block_result),
        l2block_committed_info: L2BlockCommittedInfo::new_builder()
            .number(2u64.pack())
            .build(),
    };
    let param = SyncParam {
        updates: vec![action2.clone()],
        reverts: Default::default(),
    };
    chain.sync(param).unwrap();
    assert_eq!(chain.last_sync_event().is_success(), true);
    let tip_block = chain.store().get_tip_block().unwrap();
    let tip_block_number: u64 = tip_block.raw().number().unpack();
    assert_eq!(tip_block_number, 2);

    // revert blocks
    let updates = vec![action2.clone()];
    let reverts = updates
        .into_iter()
        .rev()
        .map(|action| {
            let prev_global_state = GlobalState::default();
            let L1Action {
                transaction: _,
                l2block_committed_info,
                context,
            } = action;
            let l2block = match context {
                L1ActionContext::SubmitBlock { l2block, .. } => l2block,
                _ => unreachable!(),
            };
            let context = RevertL1ActionContext::SubmitValidBlock { l2block };
            RevertedL1Action {
                prev_global_state,
                l2block_committed_info,
                context,
            }
        })
        .collect::<Vec<_>>();

    let param = SyncParam {
        updates: Default::default(),
        reverts,
    };
    chain.sync(param).unwrap();
    assert_eq!(chain.last_sync_event().is_success(), true);

    let tip_block = chain.store().get_tip_block().unwrap();
    let tip_block_number: u64 = tip_block.raw().number().unpack();
    assert_eq!(tip_block_number, 1);

    // check account SMT, should be able to calculate account state root
    {
        let db = chain.store().begin_transaction();
        let db = StateDBTransaction::from_checkpoint(
            &db,
            CheckPoint::from_block_hash(&db, tip_block.hash().into(), SubState::Block).unwrap(),
            StateDBMode::ReadOnly,
        )
        .unwrap();
        let tree = db.account_state_tree().unwrap();
        let current_account_root = tree.calculate_root().unwrap();
        let expected_account_root: H256 = tip_block.raw().post_account().merkle_root().unpack();
        assert_eq!(
            current_account_root, expected_account_root,
            "check account tree"
        );

        assert_eq!(tree.get_account_count().unwrap(), 3);
        let alice_script_hash: H256 = alice_script.hash().into();
        let alice_id = tree
            .get_account_id_by_script_hash(&alice_script_hash)
            .unwrap()
            .unwrap();
        assert_eq!(alice_id, 2);
        let alice_balance = tree
            .get_sudt_balance(CKB_SUDT_ACCOUNT_ID, to_short_address(&alice_script_hash))
            .unwrap();
        assert_eq!(alice_balance, 100);

        let bob_id_opt = tree
            .get_account_id_by_script_hash(&bob_script.hash().into())
            .unwrap();
        assert!(bob_id_opt.is_none());
    }

    // execute block2 agnain
    let updates = vec![action2];
    let param = SyncParam {
        updates,
        reverts: Default::default(),
    };
    chain.sync(param).unwrap();
    assert_eq!(chain.last_sync_event().is_success(), true);

    // check block2 agnain

    let tip_block = chain.store().get_tip_block().unwrap();
    let tip_block_number: u64 = tip_block.raw().number().unpack();
    assert_eq!(tip_block_number, 2);

    {
        let db = chain.store().begin_transaction();
        let db = StateDBTransaction::from_checkpoint(
            &db,
            CheckPoint::from_block_hash(&db, tip_block.hash().into(), SubState::Block).unwrap(),
            StateDBMode::ReadOnly,
        )
        .unwrap();
        let tree = db.account_state_tree().unwrap();
        let current_account_root = tree.calculate_root().unwrap();
        let expected_account_root: H256 = tip_block.raw().post_account().merkle_root().unpack();
        assert_eq!(
            current_account_root, expected_account_root,
            "check account tree"
        );

        assert_eq!(tree.get_account_count().unwrap(), 4);
        let alice_script_hash: H256 = alice_script.hash().into();
        let alice_id = tree
            .get_account_id_by_script_hash(&alice_script_hash)
            .unwrap()
            .unwrap();
        assert_eq!(alice_id, 2);
        let alice_balance = tree
            .get_sudt_balance(CKB_SUDT_ACCOUNT_ID, to_short_address(&alice_script_hash))
            .unwrap();
        assert_eq!(alice_balance, 100);

        let bob_script_hash: H256 = bob_script.hash().into();
        let bob_id = tree
            .get_account_id_by_script_hash(&bob_script_hash)
            .unwrap()
            .unwrap();
        assert_eq!(bob_id, 3);

        let bob_balance = tree
            .get_sudt_balance(CKB_SUDT_ACCOUNT_ID, to_short_address(&bob_script_hash))
            .unwrap();
        assert_eq!(bob_balance, 500);
    }
}

#[test]
fn test_sync_blocks() {
    let rollup_type_script = Script::default();
    let rollup_script_hash = rollup_type_script.hash();
    let mut chain1 = setup_chain(rollup_type_script.clone());
    let mut chain2 = setup_chain(rollup_type_script.clone());

    let rollup_cell = CellOutput::new_builder()
        .type_(Some(rollup_type_script).pack())
        .build();

    // block #1
    let user_script_a = Script::new_builder()
        .code_hash(ALWAYS_SUCCESS_CODE_HASH.clone().pack())
        .hash_type(ScriptHashType::Type.into())
        .args({
            let mut args = rollup_script_hash.to_vec();
            args.push(42);
            args.pack()
        })
        .build();
    let sudt_script_hash: H256 = [42u8; 32].into();
    let deposit = DepositRequest::new_builder()
        .capacity(100u64.pack())
        .script(user_script_a.clone())
        .sudt_script_hash(sudt_script_hash.pack())
        .build();
    let sync_1 = produce_a_block(&mut chain1, deposit, rollup_cell.clone(), 1);

    // block #2
    let deposit = DepositRequest::new_builder()
        .capacity(200u64.pack())
        .script(user_script_a.clone())
        .build();
    let sync_2 = produce_a_block(&mut chain1, deposit, rollup_cell.clone(), 2);

    // block #3
    let user_script_b = Script::new_builder()
        .code_hash(ALWAYS_SUCCESS_CODE_HASH.clone().pack())
        .hash_type(ScriptHashType::Type.into())
        .args({
            let mut args = rollup_script_hash.to_vec();
            args.push(50);
            args.pack()
        })
        .build();
    let deposit = DepositRequest::new_builder()
        .capacity(500u64.pack())
        .script(user_script_b.clone())
        .sudt_script_hash(sudt_script_hash.pack())
        .build();
    let sync_3 = produce_a_block(&mut chain1, deposit, rollup_cell.clone(), 3);

    drop(chain1);

    chain2.sync(sync_1).expect("success");
    assert_eq!(chain2.last_sync_event().is_success(), true);

    chain2.sync(sync_2).expect("success");
    assert_eq!(chain2.last_sync_event().is_success(), true);

    chain2.sync(sync_3).expect("success");
    assert_eq!(chain2.last_sync_event().is_success(), true);

    // check state
    {
        let db = chain2.store().begin_transaction();
        let tip_block = db.get_tip_block().unwrap();
        let tip_block_number: u64 = tip_block.raw().number().unpack();
        let tip_block_hash = db.get_tip_block_hash().unwrap();
        assert_eq!(tip_block_hash, tip_block.hash().into());
        assert_eq!(tip_block_number, 3);

        let state_db = StateDBTransaction::from_checkpoint(
            &db,
            CheckPoint::from_block_hash(&db, tip_block_hash, SubState::Block).unwrap(),
            StateDBMode::ReadOnly,
        )
        .unwrap();
        let tree = state_db.account_state_tree().unwrap();
        let script_hash_a: H256 = user_script_a.hash().into();
        let id_a = tree
            .get_account_id_by_script_hash(&script_hash_a)
            .unwrap()
            .unwrap();
        let script_hash_b: H256 = user_script_b.hash().into();
        let id_b = tree
            .get_account_id_by_script_hash(&script_hash_b)
            .unwrap()
            .unwrap();
        // 0 is meta contract, 1 is ckb sudt, so the user id start from 2
        assert_eq!(id_a, 2);
        assert_eq!(id_b, 4);
        let balance_a = tree
            .get_sudt_balance(CKB_SUDT_ACCOUNT_ID, to_short_address(&script_hash_a))
            .unwrap();
        let balance_b = tree
            .get_sudt_balance(CKB_SUDT_ACCOUNT_ID, to_short_address(&script_hash_b))
            .unwrap();
        assert_eq!(balance_a, 300);
        assert_eq!(balance_b, 500);
    }

    drop(chain2);
}
