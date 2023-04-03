use crate::burnchains::Txid;
use crate::chainstate::stacks::StacksMicroblockHeader;
use crate::chainstate::stacks::StacksTransaction;
use crate::codec::StacksMessageCodec;
use crate::types::chainstate::StacksAddress;
use clarity::util::hash::to_hex;
use clarity::vm::analysis::ContractAnalysis;
use clarity::vm::costs::ExecutionCost;
use clarity::vm::database::ClaritySerializable;
use clarity::vm::types::{
    AssetIdentifier, PrincipalData, QualifiedContractIdentifier, StandardPrincipalData, Value,
};
use std::io::{Read, Write};

use crate::chainstate::burn::operations::BlockstackOperationType;
pub use clarity::vm::events::StacksTransactionEvent;
use stacks_common::codec::*;

#[derive(Debug, Clone, PartialEq)]
pub enum TransactionOrigin {
    Stacks(StacksTransaction),
    Burn(BlockstackOperationType),
}

impl From<StacksTransaction> for TransactionOrigin {
    fn from(o: StacksTransaction) -> TransactionOrigin {
        TransactionOrigin::Stacks(o)
    }
}

impl TransactionOrigin {
    pub fn txid(&self) -> Txid {
        match self {
            TransactionOrigin::Burn(op) => op.txid(),
            TransactionOrigin::Stacks(tx) => tx.txid(),
        }
    }
    /// Serialize this origin type to a string that can be stored in
    ///  a database
    pub fn serialize_to_dbstring(&self) -> String {
        match self {
            TransactionOrigin::Burn(op) => format!("BTC({})", op.txid()),
            TransactionOrigin::Stacks(tx) => to_hex(&tx.serialize_to_vec()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StacksTransactionReceipt {
    pub transaction: TransactionOrigin,
    pub events: Vec<StacksTransactionEvent>,
    pub post_condition_aborted: bool,
    pub result: Value,
    pub stx_burned: u128,
    pub contract_analysis: Option<ContractAnalysis>,
    pub execution_cost: ExecutionCost,
    pub microblock_header: Option<StacksMicroblockHeader>,
    pub tx_index: u32,
    /// This is really a string-formatted CheckError (which can't be clone()'ed)
    pub vm_error: Option<String>,
}

impl StacksMessageCodec for StacksTransactionReceipt {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), Error>
    where
        Self: Sized,
    {
        match &self.transaction {
            TransactionOrigin::Stacks(tx) => {
                write_next(fd, &0u8)?;
                write_next(fd, tx)?;
            }
            TransactionOrigin::Burn(tx) => {
                write_next(fd, &1u8)?;
                write_next(fd, &tx.blockstack_op_to_json().to_string().into_bytes())?;
            }
        }
        let mut events_json: Vec<Vec<u8>> = vec![];
        for i in 0..self.events.len() {
            let evt = &self.events[i];
            let evt_json = evt
                .json_serialize(i, &self.transaction.txid(), true)
                .to_string();
            events_json.push(evt_json.into_bytes());
        }
        write_next(fd, &events_json)?;
        write_next(fd, &self.post_condition_aborted)?;
        write_next(fd, &self.result.serialize_to_vec())?;
        write_next(fd, &self.stx_burned)?;
        write_next(
            fd,
            &self
                .contract_analysis
                .as_ref()
                .map(|c| c.serialize().into_bytes()),
        )?;
        write_next(fd, &self.execution_cost)?;
        write_next(fd, &self.microblock_header)?;
        write_next(fd, &self.tx_index)?;
        write_next(fd, &self.vm_error.as_ref().map(|e| e.clone().into_bytes()))?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(_fd: &mut R) -> Result<Self, Error>
    where
        Self: Sized,
    {
        // StacksTransactionEvent can be serialized into json, but not the other way around yet.
        todo!()
    }
}
