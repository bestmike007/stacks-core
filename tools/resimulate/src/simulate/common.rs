use clarity::vm::Value;
use clarity::vm::costs::ExecutionCost;
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub enum SimulationReadStep {
    MapEntry(QualifiedContractIdentifier, String, Value),
    DataVar(QualifiedContractIdentifier, String),
    EvalReadonly(
        PrincipalData,
        Option<PrincipalData>,
        QualifiedContractIdentifier,
        String,
    ),
    StxBalance(PrincipalData),
    FtBalance(QualifiedContractIdentifier, String, PrincipalData),
    FtSupply(QualifiedContractIdentifier, String),
    Nonce(PrincipalData),
}

#[derive(Deserialize, Serialize, Debug)]
pub struct SimulationTransactionReceipt {
    pub events: Vec<String>,
    pub post_condition_aborted: bool,
    pub result: String,
    pub stx_burned: u128,
    pub execution_cost: ExecutionCost,
    pub costs: u32,
    pub tx_index: u32,
    pub vm_error: Option<String>,
}
