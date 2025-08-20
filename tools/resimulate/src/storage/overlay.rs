use blockstack_lib::chainstate::stacks::index::Error;
use blockstack_lib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts, MarfConnection};
use blockstack_lib::chainstate::stacks::index::storage::{
    TrieFileStorage, TrieHashCalculationMode,
};
use blockstack_lib::core::{FIRST_BURNCHAIN_CONSENSUS_HASH, FIRST_STACKS_BLOCK_HASH};
use clarity::types::chainstate::StacksBlockId;
use clarity::vm::database::clarity_store::{ContractCommitment, make_contract_hash_key};
use clarity::vm::database::{ClarityBackingStore, ClarityDeserializable, SqliteConnection};
use clarity::vm::errors::{CheckErrors, InterpreterError};
use clarity::vm::types::QualifiedContractIdentifier;
use hashbrown::HashMap;
use rusqlite::Connection;

pub struct InMemoryOverlayClarityBackingStore {
    marf: MARF<StacksBlockId>,
    parent_block: StacksBlockId,
    tip_block: StacksBlockId,
    current_block: StacksBlockId,
    override_storage: HashMap<String, String>,
    override_metadata: HashMap<(QualifiedContractIdentifier, String), String>,
}

impl InMemoryOverlayClarityBackingStore {
    pub fn new(
        marf_path: &str,
        parent_block: StacksBlockId,
        new_block: StacksBlockId,
    ) -> InMemoryOverlayClarityBackingStore {
        let trie = TrieFileStorage::open_readonly(
            marf_path,
            MARFOpenOpts {
                hash_calculation_mode: TrieHashCalculationMode::Deferred,
                cache_strategy: "noop".to_string(),
                external_blobs: true,
                force_db_migrate: false,
            },
        )
        .unwrap();
        let mut marf: MARF<StacksBlockId> = MARF::from_storage(trie);
        marf.open_block(&parent_block).unwrap();
        InMemoryOverlayClarityBackingStore {
            marf,
            parent_block,
            tip_block: new_block,
            current_block: new_block,
            override_metadata: HashMap::new(),
            override_storage: HashMap::new(),
        }
    }
}

impl ClarityBackingStore for InMemoryOverlayClarityBackingStore {
    fn put_all_data(
        &mut self,
        items: Vec<(String, String)>,
    ) -> clarity::vm::errors::InterpreterResult<()> {
        for (key, value) in items.into_iter() {
            self.override_storage.insert(key, value);
        }
        Ok(())
    }

    fn get_data(&mut self, key: &str) -> clarity::vm::errors::InterpreterResult<Option<String>> {
        if self.current_block == self.tip_block && self.override_storage.contains_key(key) {
            return Ok(self.override_storage.get(key).map(|v| v.clone()));
        }
        let result = self
            .marf
            .get(
                if self.current_block == self.tip_block {
                    &self.parent_block
                } else {
                    &self.current_block
                },
                key,
            )
            .or_else(|e| match e {
                Error::NotFoundError => {
                    // trace!(
                    //     "MarfedKV get {:?} off of {:?}: not found",
                    //     key, &self.chain_tip
                    // );
                    Ok(None)
                }
                _ => Err(e),
            })
            .map_err(|_| InterpreterError::Expect("ERROR: Unexpected MARF Failure on GET".into()))?
            .map(|marf_value| {
                let side_key = marf_value.to_hex();
                // trace!("MarfedKV get side-key for {:?}: {:?}", key, &side_key);
                SqliteConnection::get(self.marf.sqlite_conn(), &side_key)?.ok_or_else(|| {
                    InterpreterError::Expect(format!(
                        "ERROR: MARF contained value_hash not found in side storage: {}",
                        side_key
                    ))
                    .into()
                })
            })
            .transpose();
        if result.is_ok() {
            let v = result.as_ref().unwrap().as_ref();
            if self.current_block == self.tip_block && v.is_some() {
                self.override_storage
                    .insert(key.to_string(), v.unwrap().clone());
            }
        }
        result
    }

    fn get_data_with_proof(
        &mut self,
        _key: &str,
    ) -> clarity::vm::errors::InterpreterResult<Option<(String, Vec<u8>)>> {
        todo!()
    }

    fn set_block_hash(
        &mut self,
        bhh: StacksBlockId,
    ) -> clarity::vm::errors::InterpreterResult<StacksBlockId> {
        let prev_block = self.current_block;
        if bhh == self.tip_block {
            self.marf.open_block(&self.parent_block).unwrap();
            self.current_block = self.tip_block;
            return Ok(prev_block);
        }
        self.marf.open_block(&bhh).unwrap();
        self.current_block = bhh;
        Ok(prev_block)
    }

    fn get_block_at_height(&mut self, height: u32) -> Option<StacksBlockId> {
        self.marf
            .get_bhh_at_height(
                if self.current_block == self.tip_block {
                    &self.parent_block
                } else {
                    &self.current_block
                },
                height,
            )
            .unwrap()
    }

    fn get_current_block_height(&mut self) -> u32 {
        if self.current_block == self.tip_block {
            return self.get_open_chain_tip_height();
        }
        match self
            .marf
            .get_block_height_of(&self.current_block, &self.current_block)
        {
            Ok(Some(x)) => x,
            Ok(None) => {
                let first_tip =
                    StacksBlockId::new(&FIRST_BURNCHAIN_CONSENSUS_HASH, &FIRST_STACKS_BLOCK_HASH);
                if self.tip_block == first_tip || self.tip_block == StacksBlockId([0u8; 32]) {
                    // the current block height should always work, except if it's the first block
                    // height (in which case, the current chain tip should match the first-ever
                    // index block hash).
                    return 0;
                }

                // should never happen
                let msg = format!(
                    "Failed to obtain current block height of {} (got None)",
                    &self.tip_block
                );
                panic!("{}", &msg);
            }
            Err(e) => {
                let msg = format!(
                    "Unexpected MARF failure: Failed to get current block height of {}: {:?}",
                    &self.tip_block, &e
                );
                panic!("{}", &msg);
            }
        }
    }

    fn get_open_chain_tip_height(&mut self) -> u32 {
        match self
            .marf
            .get_block_height_of(&self.parent_block, &self.parent_block)
        {
            Ok(Some(x)) => x + 1,
            Ok(None) => {
                let first_tip =
                    StacksBlockId::new(&FIRST_BURNCHAIN_CONSENSUS_HASH, &FIRST_STACKS_BLOCK_HASH);
                if self.tip_block == first_tip || self.tip_block == StacksBlockId([0u8; 32]) {
                    // the current block height should always work, except if it's the first block
                    // height (in which case, the current chain tip should match the first-ever
                    // index block hash).
                    return 0;
                }

                // should never happen
                let msg = format!(
                    "Failed to obtain current block height of {} (got None)",
                    &self.tip_block
                );
                panic!("{}", &msg);
            }
            Err(e) => {
                let msg = format!(
                    "Unexpected MARF failure: Failed to get current block height of {}: {:?}",
                    &self.tip_block, &e
                );
                panic!("{}", &msg);
            }
        }
    }

    fn get_open_chain_tip(&mut self) -> StacksBlockId {
        self.tip_block.clone()
    }

    fn get_side_store(&mut self) -> &Connection {
        todo!()
    }

    fn get_contract_hash(
        &mut self,
        contract: &clarity::vm::types::QualifiedContractIdentifier,
    ) -> clarity::vm::errors::InterpreterResult<(
        StacksBlockId,
        clarity::util::hash::Sha512Trunc256Sum,
    )> {
        let key = make_contract_hash_key(contract);
        let contract_commitment = self
            .get_data(&key)?
            .map(|x| ContractCommitment::deserialize(&x))
            .ok_or_else(|| CheckErrors::NoSuchContract(contract.to_string()))?;
        let ContractCommitment {
            block_height,
            hash: contract_hash,
        } = contract_commitment?;
        let bhh = self.get_block_at_height(block_height)
              .ok_or_else(|| InterpreterError::Expect("Should always be able to map from height to block hash when looking up contract information.".into()))?;
        Ok((bhh, contract_hash))
    }

    fn insert_metadata(
        &mut self,
        contract: &clarity::vm::types::QualifiedContractIdentifier,
        key: &str,
        value: &str,
    ) -> clarity::vm::errors::InterpreterResult<()> {
        self.override_metadata
            .insert((contract.clone(), key.to_string()), value.to_string());
        Ok(())
    }

    fn get_metadata(
        &mut self,
        contract: &clarity::vm::types::QualifiedContractIdentifier,
        key: &str,
    ) -> clarity::vm::errors::InterpreterResult<Option<String>> {
        let override_key = (contract.clone(), key.to_string());
        if self.current_block == self.tip_block
            && self.override_metadata.contains_key(&override_key)
        {
            return Ok(self.override_metadata.get(&override_key).map(|v| v.clone()));
        }
        let (bhh, _) = self.get_contract_hash(contract)?;
        SqliteConnection::get_metadata(self.marf.sqlite_conn(), &bhh, &contract.to_string(), key)
    }

    fn get_metadata_manual(
        &mut self,
        _at_height: u32,
        _contract: &clarity::vm::types::QualifiedContractIdentifier,
        _key: &str,
    ) -> clarity::vm::errors::InterpreterResult<Option<String>> {
        todo!()
    }

    fn get_data_from_path(
        &mut self,
        _hash: &clarity::types::chainstate::TrieHash,
    ) -> clarity::vm::errors::InterpreterResult<Option<String>> {
        todo!()
    }

    fn get_data_with_proof_from_path(
        &mut self,
        _hash: &clarity::types::chainstate::TrieHash,
    ) -> clarity::vm::errors::InterpreterResult<Option<(String, Vec<u8>)>> {
        todo!()
    }
}
