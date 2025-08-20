use blockstack_lib::chainstate::burn::db::sortdb::SortitionDB;
use blockstack_lib::chainstate::nakamoto::{
    NakamotoBlock, NakamotoChainState, NakamotoStagingBlocksConn,
};
use blockstack_lib::chainstate::stacks::db::blocks::StagingBlock;
use blockstack_lib::chainstate::stacks::db::{DBConfig, StacksChainState, StacksHeaderInfo};
use blockstack_lib::chainstate::stacks::events::StacksTransactionReceipt;
use blockstack_lib::chainstate::stacks::index::marf::{MARF, MarfConnection};
use blockstack_lib::chainstate::stacks::{StacksTransaction, TransactionPayload};
use blockstack_lib::clarity_vm::clarity::ClarityTransactionConnection;
use blockstack_lib::util_lib::db::{query_row, query_rows, u64_to_sql};
use clarity::codec::StacksMessageCodec;
use clarity::types::StacksEpochId;
use clarity::types::chainstate::{ConsensusHash, StacksBlockId};
use clarity::vm::ClarityVersion;
use clarity::vm::ast::ASTRules;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::database::{BurnStateDB, HeadersDB};
use rusqlite::params;
use stacks_common::types::chainstate::BlockHeaderHash;

// copy from stackslib/src/chainstate/stacks/db/transactions.rs and modified to use readonly connections
pub fn process_transaction(
    store: &mut dyn clarity::vm::database::ClarityBackingStore,
    header_db: &dyn HeadersDB,
    burn_state_db: &dyn BurnStateDB,
    cost_track: &mut Option<LimitedCostTracker>,
    config: &DBConfig,
    epoch: StacksEpochId,
    ast_rules: ASTRules,
    tx: &StacksTransaction,
) -> Result<StacksTransactionReceipt, String> {
    StacksChainState::process_transaction_precheck(config, tx, epoch)
        .map_err(|e| format!("{:?}", e))?;
    if epoch < StacksEpochId::Epoch21 {
        let tx_clarity_version = match &tx.payload {
            TransactionPayload::SmartContract(_, version_opt) => {
                // did the caller want to run a particular version of Clarity?
                version_opt.unwrap_or(ClarityVersion::default_for_epoch(epoch))
            }
            _ => {
                // whatever the epoch default is, since no Clarity code will be executed anyway
                ClarityVersion::default_for_epoch(epoch)
            }
        };
        if tx_clarity_version == ClarityVersion::Clarity2 {
            let msg = format!(
                "Invalid transaction {}: asks for Clarity2, but not in Stacks epoch 2.1 or later",
                tx.txid()
            );
            return Err(msg);
        }
    }
    let mut clarity_tx = ClarityTransactionConnection::new(
        store,
        header_db,
        burn_state_db,
        cost_track,
        config.mainnet,
        config.chain_id,
        epoch,
    );
    let fee = tx.get_tx_fee();
    let tx_receipt = if epoch >= StacksEpochId::Epoch21 {
        // 2.1 and later: pay tx fee, then process transaction
        let (_origin_account, payer_account) =
            StacksChainState::check_transaction_nonces(&mut clarity_tx, tx, false)
                .map_err(|e| format!("{:?}", e))?;

        let payer_address = payer_account.principal.clone();
        let payer_nonce = payer_account.nonce;
        StacksChainState::pay_transaction_fee(&mut clarity_tx, fee, payer_account)
            .map_err(|e| format!("{:?}", e))?;

        // origin balance may have changed (e.g. if the origin paid the tx fee), so reload the account
        let origin_account =
            StacksChainState::get_account(&mut clarity_tx, &tx.origin_address().into());

        let tx_receipt = StacksChainState::process_transaction_payload(
            &mut clarity_tx,
            tx,
            &origin_account,
            ast_rules,
            None,
        )
        .map_err(|e| match e {
            blockstack_lib::chainstate::stacks::Error::CostOverflowError(_, _, _) => {
                "CostOverflowError".to_owned()
            }
            _ => format!("{:?}", e),
        })?;

        // update the account nonces
        StacksChainState::update_account_nonce(
            &mut clarity_tx,
            &origin_account.principal,
            origin_account.nonce,
        );
        if origin_account.principal != payer_address {
            // payer is a different account, so update its nonce too
            StacksChainState::update_account_nonce(&mut clarity_tx, &payer_address, payer_nonce);
        }

        tx_receipt
    } else {
        // pre-2.1: process transaction, then pay tx fee
        let (origin_account, payer_account) =
            StacksChainState::check_transaction_nonces(&mut clarity_tx, tx, false)
                .map_err(|e| format!("{:?}", e))?;

        let tx_receipt = StacksChainState::process_transaction_payload(
            &mut clarity_tx,
            tx,
            &origin_account,
            ast_rules,
            None,
        )
        .map_err(|e| format!("{:?}", e))?;

        let new_payer_account = StacksChainState::get_payer_account(&mut clarity_tx, tx);
        StacksChainState::pay_transaction_fee(&mut clarity_tx, fee, new_payer_account)
            .map_err(|e| format!("{:?}", e))?;

        // update the account nonces
        StacksChainState::update_account_nonce(
            &mut clarity_tx,
            &origin_account.principal,
            origin_account.nonce,
        );
        if origin_account != payer_account {
            StacksChainState::update_account_nonce(
                &mut clarity_tx,
                &payer_account.principal,
                payer_account.nonce,
            );
        }

        tx_receipt
    };

    clarity_tx.commit().map_err(|e| format!("{:?}", e))?;
    Ok(tx_receipt)
}

pub fn load_block_header(
    state_index: &MARF<StacksBlockId>,
    consensus_hash: &ConsensusHash,
    anchored_block_hash: &BlockHeaderHash,
) -> Result<StacksHeaderInfo, String> {
    let block_header = StacksChainState::get_anchored_block_header_info(
        state_index.sqlite_conn(),
        &consensus_hash,
        &anchored_block_hash,
    )
    .map_err(|e| format!("failed to load block header, {:?}", e))?;
    if block_header.is_none() {
        return Err("failed to load block header".to_owned());
    }
    Ok(block_header.unwrap())
}

pub fn get_anchored_block_header(
    state_index: &MARF<StacksBlockId>,
    nakamoto_staging_blocks_conn: &NakamotoStagingBlocksConn,
    block_height: u64,
    block_hash: &BlockHeaderHash,
) -> Result<(StacksHeaderInfo, StacksBlockId), String> {
    let sql = "SELECT block_hash,index_block_hash,parent_block_id FROM nakamoto_staging_blocks WHERE height = ?1 AND orphaned = 0 AND processed = 1";
    let mut stmt = nakamoto_staging_blocks_conn
        .prepare(sql)
        .map_err(|e| format!("failed to prepare query, {:?}", e))?;
    let mut qry = stmt
        .query(&[&(block_height as i64)])
        .map_err(|e| format!("failed to execute query, {:?}", e))?;
    while let Some(row) = qry
        .next()
        .map_err(|e| format!("failed to get next row, {:?}", e))?
    {
        let record_block_hash: BlockHeaderHash = row
            .get(0)
            .map_err(|e| format!("failed to get block hash from row, {:?}", e))?;
        if record_block_hash != *block_hash {
            continue;
        }

        let index_block_hash: StacksBlockId = row
            .get(1)
            .map_err(|e| format!("failed to get index block hash from row, {:?}", e))?;

        let parent_block_id: StacksBlockId = row
            .get(2)
            .map_err(|e| format!("failed to get parent block id from row, {:?}", e))?;

        let block =
            NakamotoChainState::get_block_header(state_index.sqlite_conn(), &index_block_hash)
                .map_err(|e| format!("failed to get nakamoto block header, {:?}", e))?;
        if block.is_none() {
            return Err(format!(
                "block height {} with block hash {:?} not found",
                block_height, block_hash
            ));
        }
        return Ok((block.unwrap(), parent_block_id));
    }
    let blocks_at_height = get_stacks_chain_tips_at_height(state_index, block_height)?;
    let anchored_block = blocks_at_height
        .iter()
        .find(|b| b.anchored_block_hash.eq(&block_hash));
    if anchored_block.is_none() {
        return Err(format!(
            "block height {} with block hash {:?} not found",
            block_height, block_hash
        ));
    }
    let anchored_block = anchored_block.unwrap();
    return Ok((
        load_block_header(
            &state_index,
            &anchored_block.consensus_hash,
            &anchored_block.anchored_block_hash,
        )?,
        StacksBlockId::new(
            &anchored_block.parent_consensus_hash,
            &anchored_block.parent_anchored_block_hash,
        ),
    ));
}

#[allow(dead_code)]
pub fn get_stacks_chain_tip(
    state_index: &MARF<StacksBlockId>,
    sortdb: &SortitionDB,
) -> Result<Option<StagingBlock>, String> {
    let (consensus_hash, block_bhh) =
        SortitionDB::get_canonical_stacks_chain_tip_hash(sortdb.conn())
            .map_err(|e| format!("failed to get blocks, {:?}", e))?;
    let sql = "SELECT * FROM staging_blocks WHERE processed = 1 AND orphaned = 0 AND consensus_hash = ?1 AND anchored_block_hash = ?2";
    let args = params![consensus_hash, block_bhh];
    query_row(state_index.sqlite_conn(), sql, args)
        .map_err(|e| format!("failed to get blocks, {:?}", e))
}
pub fn get_stacks_chain_tips_at_height(
    state_index: &MARF<StacksBlockId>,
    height: u64,
) -> Result<Vec<StagingBlock>, String> {
    let sql = "SELECT * FROM staging_blocks WHERE processed = 1 AND orphaned = 0 AND height = ?1";
    let args = params![
        u64_to_sql(height)
            .map_err(|e| format!("failed to parse height as query param, {:?}", e))?
    ];
    query_rows(state_index.sqlite_conn(), sql, args)
        .map_err(|e| format!("failed to get stacks chain tips at height, {:?}", e))
}

pub fn get_nakamoto_block(
    nakamoto_staging_blocks_conn: &NakamotoStagingBlocksConn,
    index_block_hash: &StacksBlockId,
) -> Result<Option<(NakamotoBlock, u64)>, String> {
    let qry = "SELECT data FROM nakamoto_staging_blocks WHERE index_block_hash = ?1";
    let args = params![index_block_hash];
    let res: Option<Vec<u8>> = query_row(nakamoto_staging_blocks_conn, qry, args)
        .map_err(|e| format!("failed to get nakamoto block, {:?}", e))?;
    let Some(block_bytes) = res else {
        return Ok(None);
    };
    let block = NakamotoBlock::consensus_deserialize(&mut block_bytes.as_slice())
        .map_err(|e| format!("failed to deserialize nakamoto block, {:?}", e))?;
    if &block.header.block_id() != index_block_hash {
        return Err(format!(
            "Staging DB corruption: expected {}, got {}",
            index_block_hash,
            &block.header.block_id()
        ));
    }
    Ok(Some((
        block,
        u64::try_from(block_bytes.len()).expect("FATAL: block is greater than a u64"),
    )))
}
