use std::time::SystemTime;

use blockstack_lib::chainstate::burn::db::sortdb::SortitionDB;
use blockstack_lib::chainstate::nakamoto::NakamotoChainState;
use blockstack_lib::chainstate::stacks::db::{CHAINSTATE_VERSION, DBConfig, StacksDBConn};
use blockstack_lib::chainstate::stacks::{MINER_BLOCK_CONSENSUS_HASH, MINER_BLOCK_HEADER_HASH};
use blockstack_lib::core::TX_BLOCK_LIMIT_PROPORTION_HEURISTIC;
use clarity::consts::{CHAIN_ID_MAINNET, CHAIN_ID_TESTNET};
use clarity::types::chainstate::StacksBlockId;
use clarity::vm::Value;
use clarity::vm::analysis::ContractAnalysis;
use clarity::vm::costs::{ExecutionCost, LimitedCostTracker};
use clarity::vm::database::{BurnStateDB, ClarityDatabase};
use clarity::vm::types::StacksAddressExtensions;
use serde::{Deserialize, Serialize};
use stacks_common::types::chainstate::BlockHeaderHash;

use super::stacks::*;
use crate::storage::{
    InMemoryOverlayClarityBackingStore, get_state_index_path, open_nakamoto_staging_blocks,
    open_sort_db_readonly, open_state_index_readonly,
};

#[derive(Deserialize, Serialize, Debug)]
pub struct NakamotoStacksTransactionPlainReceipt {
    pub txid: [u8; 32],
    pub events: Vec<String>,
    pub post_condition_aborted: bool,
    pub result: Value,
    pub stx_burned: u128,
    pub contract_analysis: Option<ContractAnalysis>,
    pub execution_cost: ExecutionCost,
    pub cost_so_far: ExecutionCost,
    pub costs: u32,
    pub tx_index: u32,
    pub vm_error: Option<String>,
}

#[derive(Deserialize, Serialize, Debug)]
pub enum NakamotoSimulateStepResult {
    Transaction(Result<NakamotoStacksTransactionPlainReceipt, String>),
    EvalReadonly(Result<Value, String>),
    Reads(Vec<Result<String, String>>),
    SetContractCode(Result<(), String>),
    ResetCosts(ExecutionCost),
}

#[derive(Deserialize, Serialize, Debug)]
pub struct NakamotoSimulateResult {
    pub block_height: u64,
    pub block_hash: [u8; 32],
    pub index_block_hash: [u8; 32],
    pub error: Option<String>,
    pub receipts: Vec<Result<NakamotoStacksTransactionPlainReceipt, String>>,
    pub tenure_initial_costs: ExecutionCost,
}

pub fn nakamoto_simulate(
    block_height: u64,
    block_hash: &str,
) -> Result<NakamotoSimulateResult, String> {
    let state_index = open_state_index_readonly()?;
    let sortdb = open_sort_db_readonly()?;
    let nakamoto_staging_blocks_conn = open_nakamoto_staging_blocks()?;
    let (block_header, parent_block_id) = get_anchored_block_header(
        &state_index,
        &nakamoto_staging_blocks_conn,
        block_height,
        &BlockHeaderHash::from_hex(block_hash)
            .map_err(|e| format!("failed to parse block hash, {:?}", e))?,
    )?;

    let block = get_nakamoto_block(
        &nakamoto_staging_blocks_conn,
        &block_header.index_block_hash(),
    )
    .map_err(|e| format!("{}", e))?;
    if block.is_none() {
        println!(
            "failed to find nakamoto block with index block hash: {:?}",
            &block_header.index_block_hash()
        );
        return Err(format!(
            "failed to find nakamoto block with index block hash: {:?}",
            &block_header.index_block_hash()
        ));
    }
    let block = block.unwrap().0;
    println!(
        "Processing {} block hash {:?} with {} txs",
        block.header.chain_length,
        block.header.block_hash(),
        block.txs.len()
    );
    let burndb = &sortdb
        .index_handle_at_ch(&block_header.consensus_hash)
        .map_err(|e| format!("failed to get index handle, {:?}", e))?;
    let ast_rules =
        SortitionDB::get_ast_rules(&sortdb.index_conn(), block_header.burn_header_height.into())
            .map_err(|e| format!("failed to get ast rules, {:?}", e))?;

    let tenure_costs: ExecutionCost = NakamotoChainState::get_total_tenure_cost_at(
        &StacksDBConn::new(&state_index, ()),
        &block_header.index_block_hash(),
    )
    .map_err(|e| format!("failed to load total tenure cost, {:?}", e))?
    .unwrap_or(ExecutionCost::ZERO);

    let mut result = NakamotoSimulateResult {
        block_hash: block_header.anchored_header.block_hash().0.clone(),
        block_height: block_header.stacks_block_height,
        index_block_hash: block_header.index_block_hash().0.clone(),
        error: None,
        receipts: vec![],
        tenure_initial_costs: tenure_costs.clone(),
    };

    let new_bhh = StacksBlockId::new(&MINER_BLOCK_CONSENSUS_HASH, &MINER_BLOCK_HEADER_HASH);
    let epoch = burndb
        .get_stacks_epoch(block_header.burn_header_height)
        .unwrap();
    let is_testnet = std::env::var("TESTNET_MODE") == Ok("true".to_owned());
    let chain_config = &DBConfig {
        mainnet: !is_testnet,
        chain_id: if is_testnet {
            CHAIN_ID_TESTNET
        } else {
            CHAIN_ID_MAINNET
        },
        version: CHAINSTATE_VERSION.to_string(),
    };
    let mut clarity_db = InMemoryOverlayClarityBackingStore::new(
        &get_state_index_path()?,
        parent_block_id.clone(),
        new_bhh.clone(),
    );
    let mut cost_track = {
        let mut clarity_db = ClarityDatabase::new(&mut clarity_db, &state_index, burndb);
        let mut cost_track = LimitedCostTracker::new(
            chain_config.mainnet,
            chain_config.chain_id,
            epoch.block_limit.clone(),
            &mut clarity_db,
            epoch.epoch_id,
        )
        .expect("FAIL: problem instantiating cost tracking");
        cost_track.set_total(tenure_costs);
        Some(cost_track)
    };

    let mut tx_index = 0;
    let mut tenure_extend = 0;

    for tx in block.txs.iter() {
        println!(
            "Start processing tx {:?} from {}",
            tx.txid(),
            tx.auth
                .origin()
                .address_mainnet()
                .to_account_principal()
                .to_string()
        );
        tx_index += 1;
        let tx_start = SystemTime::now();
        let cost_before = cost_track.as_ref().unwrap().get_total();
        let tx_receipt = process_transaction(
            &mut clarity_db,
            &state_index,
            burndb,
            &mut cost_track,
            &chain_config,
            epoch.epoch_id,
            ast_rules,
            &tx,
        );
        if tx_receipt.is_err() {
            if tx_receipt.as_ref().unwrap_err() == "CostOverflowError" {
                let cost_proportion = epoch.block_limit.proportion_largest_dimension(&cost_before);
                if cost_proportion < TX_BLOCK_LIMIT_PROPORTION_HEURISTIC {
                    cost_track.as_mut().unwrap().set_total(cost_before.clone());
                    result
                        .receipts
                        .push(Err("Invalid transaction: CostOverflowError".to_owned()));
                } else {
                    // costs exceeds current tenure's budget, move to the next tenure.
                    tenure_extend += 1;
                    cost_track.as_mut().unwrap().set_total(ExecutionCost::ZERO);
                }
                continue;
            }
            result.receipts.push(Err(tx_receipt.unwrap_err()));
            continue;
        }
        let tx_receipt = tx_receipt.unwrap();
        let mut r = NakamotoStacksTransactionPlainReceipt {
            txid: tx.txid().0.clone(),
            events: vec![],
            post_condition_aborted: tx_receipt.post_condition_aborted,
            result: tx_receipt.result.clone(),
            stx_burned: tx_receipt.stx_burned,
            contract_analysis: tx_receipt.contract_analysis.clone(),
            execution_cost: tx_receipt.execution_cost.clone(),
            tx_index,
            vm_error: tx_receipt.vm_error.clone(),
            costs: tx_start.elapsed().unwrap().as_millis() as u32,
            cost_so_far: cost_track.as_ref().unwrap().get_total(),
        };
        for i in 0..tx_receipt.events.len() {
            r.events.push(
                serde_json::to_string(
                    &tx_receipt.events[i]
                        .json_serialize(
                            i,
                            &tx.txid(),
                            tx_receipt.vm_error.is_none() && !tx_receipt.post_condition_aborted,
                        )
                        .map_err(|e| format!("failed to serialize tx event, {:?}", e))?,
                )
                .map_err(|e| format!("failed to serialize tx event into string, {:?}", e))?,
            )
        }
        result.receipts.push(Ok(r));
    }
    println!("Tenure extend count: {}", tenure_extend);
    return Ok(result);
}
