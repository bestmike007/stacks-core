mod overlay;
// mod overlay_sqlite;

use std::path::PathBuf;

use blockstack_lib::burnchains::PoxConstants;
use blockstack_lib::chainstate::burn::db::sortdb::SortitionDB;
use blockstack_lib::chainstate::nakamoto::NakamotoStagingBlocksConn;
use blockstack_lib::chainstate::stacks::db::StacksChainState;
use blockstack_lib::chainstate::stacks::index::marf::MARF;
use clarity::types::chainstate::StacksBlockId;
use clarity::types::sqlite::NO_PARAMS;
pub use overlay::InMemoryOverlayClarityBackingStore;

pub fn get_state_index_path() -> Result<String, String> {
    let stacks_node_dir = std::env::var("STACKS_NODE_DIR").unwrap();
    Ok(format!(
        "{}/chainstate/vm/clarity/marf.sqlite",
        &stacks_node_dir
    ))
}

pub fn open_state_index_readonly() -> Result<MARF<StacksBlockId>, String> {
    let stacks_node_dir = std::env::var("STACKS_NODE_DIR").unwrap();
    let marf = StacksChainState::open_index_readonly(&format!(
        "{}/chainstate/vm/index.sqlite",
        &stacks_node_dir
    ))
    .map_err(|e| format!("failed to open state index, {:?}", e))?;
    Ok(marf)
}

pub fn open_sort_db_readonly() -> Result<SortitionDB, String> {
    let stacks_node_dir = std::env::var("STACKS_NODE_DIR").unwrap();
    let is_mainnet =
        stacks_node_dir.ends_with("/mainnet") || stacks_node_dir.ends_with("/mainnet/");
    let sortdb = SortitionDB::open(
        &format!("{}/burnchain/sortition", &stacks_node_dir),
        false,
        if is_mainnet {
            PoxConstants::mainnet_default()
        } else {
            PoxConstants::testnet_default()
        },
    )
    .map_err(|e| format!("failed to open sortition db, {:?}", e))?;
    return Ok(sortdb);
}

pub fn open_nakamoto_staging_blocks() -> Result<NakamotoStagingBlocksConn, String> {
    let stacks_node_dir = std::env::var("STACKS_NODE_DIR").unwrap();
    let path = PathBuf::from(&format!("{}/chainstate", &stacks_node_dir));
    let nakamoto_staging_blocks_path =
        StacksChainState::static_get_nakamoto_staging_blocks_path(path)
            .map_err(|e| format!("failed to get nakamoto staging blocks path, {:?}", e))?;
    let nakamoto_staging_blocks_conn =
        StacksChainState::open_nakamoto_staging_blocks(&nakamoto_staging_blocks_path, true)
            .map_err(|e| format!("failed to open nakamoto staging blocks, {:?}", e))?;
    Ok(nakamoto_staging_blocks_conn)
}

pub fn ensure_nakamoto_block_indexes() -> Result<(), String> {
    let nakamoto_staging_blocks_conn = open_nakamoto_staging_blocks()?;
    let qry = "CREATE INDEX IF NOT EXISTS nakamoto_staging_blocks_by_height ON nakamoto_staging_blocks(height);";
    nakamoto_staging_blocks_conn
        .execute(qry, NO_PARAMS)
        .map_err(|e| format!("{}", e))?;
    Ok(())
}
