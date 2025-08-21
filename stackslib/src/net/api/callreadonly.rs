// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2023 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use clarity::types::StacksEpochId;
use clarity::vm::analysis::CheckErrors;
use clarity::vm::ast::parser::v1::CLARITY_NAME_REGEX;
use clarity::vm::clarity::{ClarityConnection, TransactionConnection};
use clarity::vm::costs::{ExecutionCost, LimitedCostTracker};
use clarity::vm::errors::Error::{Interpreter, Unchecked};
use clarity::vm::errors::{Error as ClarityRuntimeError, InterpreterError};
use clarity::vm::representations::{CONTRACT_NAME_REGEX_STRING, STANDARD_PRINCIPAL_REGEX_STRING};
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier};
use clarity::vm::{ClarityName, ClarityVersion, ContractName, SymbolicExpression, Value};
use regex::{Captures, Regex};
use stacks_common::types::chainstate::StacksAddress;
use stacks_common::types::net::PeerHost;

use crate::clarity_vm::clarity::ClarityTransactionConnection;
use crate::clarity_vm::database::marf::ReadOnlyMarfStore;
use crate::net::http::{
    parse_json, Error, HttpContentType, HttpNotFound, HttpRequest, HttpRequestContents,
    HttpRequestPreamble, HttpResponse, HttpResponseContents, HttpResponsePayload,
    HttpResponsePreamble,
};
use crate::net::httpcore::{
    request, HttpPreambleExtensions, HttpRequestContentsExtensions, RPCRequestHandler,
    StacksHttpRequest, StacksHttpResponse,
};
use crate::net::{Error as NetError, StacksNodeState, TipRequest};

#[derive(Clone, Serialize, Deserialize)]
pub struct CallReadOnlyRequestBody {
    pub sender: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sponsor: Option<String>,
    pub arguments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallReadOnlyResponse {
    pub okay: bool,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
}

#[derive(Clone)]
pub struct RPCCallReadOnlyRequestHandler {
    pub maximum_call_argument_size: u32,
    read_only_call_limit: ExecutionCost,

    /// Runtime fields
    pub contract_identifier: Option<QualifiedContractIdentifier>,
    pub function: Option<ClarityName>,
    pub sender: Option<PrincipalData>,
    pub sponsor: Option<PrincipalData>,
    pub arguments: Option<Vec<Value>>,
}

impl RPCCallReadOnlyRequestHandler {
    pub fn new(maximum_call_argument_size: u32, read_only_call_limit: ExecutionCost) -> Self {
        Self {
            maximum_call_argument_size,
            read_only_call_limit,
            contract_identifier: None,
            function: None,
            sender: None,
            sponsor: None,
            arguments: None,
        }
    }
}

/// Decode the HTTP request
impl HttpRequest for RPCCallReadOnlyRequestHandler {
    fn verb(&self) -> &'static str {
        "POST"
    }

    fn path_regex(&self) -> Regex {
        Regex::new(&format!(
            "^/v2/contracts/call-read/(?P<address>{})/(?P<contract>{})/(?P<function>{})$",
            *STANDARD_PRINCIPAL_REGEX_STRING, *CONTRACT_NAME_REGEX_STRING, *CLARITY_NAME_REGEX
        ))
        .unwrap()
    }

    fn metrics_identifier(&self) -> &str {
        "/v2/contracts/call-read/:principal/:contract_name/:func_name"
    }

    /// Try to decode this request.
    fn try_parse_request(
        &mut self,
        preamble: &HttpRequestPreamble,
        captures: &Captures,
        query: Option<&str>,
        body: &[u8],
    ) -> Result<HttpRequestContents, Error> {
        let content_len = preamble.get_content_length();
        if !(content_len > 0 && content_len < self.maximum_call_argument_size) {
            return Err(Error::DecodeError(format!(
                "Invalid Http request: invalid body length for CallReadOnly ({})",
                content_len
            )));
        }

        if preamble.content_type != Some(HttpContentType::JSON) {
            return Err(Error::DecodeError(
                "Invalid content-type: expected application/json".to_string(),
            ));
        }

        let contract_identifier = request::get_contract_address(captures, "address", "contract")?;
        let function = request::get_clarity_name(captures, "function")?;
        let body: CallReadOnlyRequestBody = serde_json::from_slice(body)
            .map_err(|_e| Error::DecodeError("Failed to parse JSON body".into()))?;

        let sender = PrincipalData::parse(&body.sender)
            .map_err(|_e| Error::DecodeError("Failed to parse sender principal".into()))?;

        let sponsor = if let Some(sponsor) = body.sponsor {
            Some(
                PrincipalData::parse(&sponsor)
                    .map_err(|_e| Error::DecodeError("Failed to parse sponsor principal".into()))?,
            )
        } else {
            None
        };

        // arguments must be valid Clarity values
        let arguments = body
            .arguments
            .into_iter()
            .map(|hex| Value::try_deserialize_hex_untyped(&hex).ok())
            .collect::<Option<Vec<Value>>>()
            .ok_or_else(|| Error::DecodeError("Failed to deserialize argument value".into()))?;

        self.contract_identifier = Some(contract_identifier);
        self.function = Some(function);
        self.sender = Some(sender);
        self.sponsor = sponsor;
        self.arguments = Some(arguments);

        Ok(HttpRequestContents::new().query_string(query))
    }
}

/// Handle the HTTP request
impl RPCRequestHandler for RPCCallReadOnlyRequestHandler {
    /// Reset internal state
    fn restart(&mut self) {
        self.contract_identifier = None;
        self.function = None;
        self.sender = None;
        self.sponsor = None;
        self.arguments = None;
    }

    /// Make the response
    fn try_handle_request(
        &mut self,
        preamble: HttpRequestPreamble,
        contents: HttpRequestContents,
        node: &mut StacksNodeState,
    ) -> Result<(HttpResponsePreamble, HttpResponseContents), NetError> {
        let tip = match node.load_stacks_chain_tip(&preamble, &contents) {
            Ok(tip) => tip,
            Err(error_resp) => {
                return error_resp.try_into_contents().map_err(NetError::from);
            }
        };

        let contract_identifier = self
            .contract_identifier
            .take()
            .ok_or(NetError::SendError("Missing `contract_identifier`".into()))?;
        let function = self
            .function
            .take()
            .ok_or(NetError::SendError("Missing `function`".into()))?;
        let sender = self
            .sender
            .take()
            .ok_or(NetError::SendError("Missing `sender`".into()))?;
        let sponsor = self.sponsor.clone();
        let arguments = self
            .arguments
            .take()
            .ok_or(NetError::SendError("Missing `arguments`".into()))?;

        // run the read-only call
        let data_resp =
            node.with_node_state(|_network, sortdb, chainstate, _mempool, _rpc_args| {
                let args: Vec<_> = arguments
                    .iter()
                    .map(|x| SymbolicExpression::atom_value(x.clone()))
                    .collect();

                let burndb = &sortdb
                    .index_handle_at_block(chainstate, &tip)
                    .map_err(|_| {
                        ClarityRuntimeError::from(InterpreterError::MarfFailure(
                            "Chain tip not found".to_string(),
                        ))
                    })?;
                let mainnet = chainstate.mainnet;
                let chain_id = chainstate.chain_id;
                let mut cost_limit = self.read_only_call_limit.clone();
                cost_limit.write_length = 0;
                cost_limit.write_count = 0;
                let (epoch, mut cost_track, _clarity_version) = chainstate
                    .with_read_only_clarity_tx(burndb, &tip, |clarity_tx| {
                        let epoch = clarity_tx.get_epoch();
                        let cost_track = clarity_tx
                            .with_clarity_db_readonly(|clarity_db| {
                                LimitedCostTracker::new_mid_block(
                                    mainnet, chain_id, cost_limit, clarity_db, epoch,
                                )
                            })
                            .map_err(|_| {
                                ClarityRuntimeError::from(InterpreterError::CostContractLoadFailure)
                            })?;

                        // TODO: not necessary to load the clarity version here?
                        let clarity_version = clarity_tx
                            .with_analysis_db_readonly(|analysis_db| {
                                analysis_db.get_clarity_version(&contract_identifier)
                            })
                            .map_err(|_| {
                                ClarityRuntimeError::from(CheckErrors::NoSuchContract(format!(
                                    "{}",
                                    &contract_identifier
                                )))
                            })?;
                        Ok::<
                            (StacksEpochId, Option<LimitedCostTracker>, ClarityVersion),
                            ClarityRuntimeError,
                        >((epoch, Some(cost_track), clarity_version))
                    })
                    .ok_or(ClarityRuntimeError::from(InterpreterError::MarfFailure(
                        "Chain tip not found".to_string(),
                    )))??;
                chainstate.clarity_state.with_marf(|marf| {
                    let mut clarity_db = ReadOnlyMarfStore::from_marf(marf, tip.clone());
                    let mut clarity_tx = ClarityTransactionConnection::new(
                        &mut clarity_db,
                        &chainstate.state_index,
                        burndb,
                        &mut cost_track,
                        mainnet,
                        chain_id,
                        epoch,
                    );
                    Ok::<Result<Value, clarity::vm::errors::Error>, ClarityRuntimeError>(
                        clarity_tx
                            .with_abort_callback(
                                |vm_env| {
                                    vm_env
                                        .execute_transaction(
                                            sender,
                                            sponsor,
                                            contract_identifier,
                                            function.as_str(),
                                            &args,
                                        )
                                        .map_err(clarity::vm::errors::Error::from)
                                },
                                |_, _| Some("read-only".to_string()),
                            )
                            .map(|v| v.0),
                    )
                })?
            });

        // decode the response
        let data_resp = match data_resp {
            Ok(data) => {
                let hex_result = data
                    .serialize_to_hex()
                    .map_err(|e| NetError::SerializeError(format!("{:?}", &e)))?;

                CallReadOnlyResponse {
                    okay: true,
                    result: Some(format!("0x{}", hex_result)),
                    cause: None,
                }
            }
            Err(e) => match e {
                Unchecked(CheckErrors::CostBalanceExceeded(actual_cost, _))
                    if actual_cost.write_count > 0 =>
                {
                    CallReadOnlyResponse {
                        okay: false,
                        result: None,
                        cause: Some("NotReadOnly".to_string()),
                    }
                }
                Interpreter(InterpreterError::MarfFailure(e)) if &e == "Chain tip not found" => {
                    return StacksHttpResponse::new_error(
                        &preamble,
                        &HttpNotFound::new("Chain tip not found".to_string()),
                    )
                    .try_into_contents()
                    .map_err(NetError::from);
                }
                _ => CallReadOnlyResponse {
                    okay: false,
                    result: None,
                    cause: Some(e.to_string()),
                },
            },
        };

        let mut preamble = HttpResponsePreamble::ok_json(&preamble);
        preamble.set_canonical_stacks_tip_height(Some(node.canonical_stacks_tip_height()));
        let body = HttpResponseContents::try_from_json(&data_resp)?;
        Ok((preamble, body))
    }
}

/// Decode the HTTP response
impl HttpResponse for RPCCallReadOnlyRequestHandler {
    fn try_parse_response(
        &self,
        preamble: &HttpResponsePreamble,
        body: &[u8],
    ) -> Result<HttpResponsePayload, Error> {
        let map_entry: CallReadOnlyResponse = parse_json(preamble, body)?;
        Ok(HttpResponsePayload::try_from_json(map_entry)?)
    }
}

impl StacksHttpRequest {
    /// Make a new request to run a read-only function
    pub fn new_callreadonlyfunction(
        host: PeerHost,
        contract_addr: StacksAddress,
        contract_name: ContractName,
        sender: PrincipalData,
        sponsor: Option<PrincipalData>,
        function_name: ClarityName,
        function_args: Vec<Value>,
        tip_req: TipRequest,
    ) -> StacksHttpRequest {
        StacksHttpRequest::new_for_peer(
            host,
            "POST".into(),
            format!(
                "/v2/contracts/call-read/{}/{}/{}",
                &contract_addr, &contract_name, &function_name
            ),
            HttpRequestContents::new().for_tip(tip_req).payload_json(
                serde_json::to_value(CallReadOnlyRequestBody {
                    sender: sender.to_string(),
                    sponsor: sponsor.map(|s| s.to_string()),
                    arguments: function_args.into_iter().map(|v| v.to_string()).collect(),
                })
                .expect("FATAL: failed to encode infallible data"),
            ),
        )
        .expect("FATAL: failed to construct request from infallible data")
    }
}

impl StacksHttpResponse {
    pub fn decode_call_readonly_response(self) -> Result<CallReadOnlyResponse, NetError> {
        let contents = self.get_http_payload_ok()?;
        let contents_json: serde_json::Value = contents.try_into()?;
        let resp: CallReadOnlyResponse = serde_json::from_value(contents_json)
            .map_err(|_e| NetError::DeserializeError("Failed to load from JSON".to_string()))?;
        Ok(resp)
    }
}
