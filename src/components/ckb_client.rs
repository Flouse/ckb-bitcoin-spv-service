//! Expand the functionality of the original CKB RPC client.

use std::collections::HashMap;

use ckb_bitcoin_spv_verifier::types::{
    core::{SpvClient, SpvInfo},
    packed,
    prelude::Unpack as VUnpack,
};
use ckb_jsonrpc_types::TransactionView;
use ckb_sdk::{
    rpc::{
        ckb_indexer::{Order, SearchKey},
        CkbRpcClient,
    },
    traits::{CellQueryOptions, LiveCell, PrimaryScriptType},
};
use ckb_types::{
    packed::{Script, Transaction},
    prelude::*,
    H256,
};

use crate::result::{Error, Result};

#[derive(Clone)]
pub struct SpvInfoCell {
    pub(crate) info: SpvInfo,
    pub(crate) cell: LiveCell,
    pub(crate) clients_count: u8,
}

#[derive(Clone)]
pub struct SpvClientCell {
    pub(crate) client: SpvClient,
    pub(crate) cell: LiveCell,
}

pub struct SpvInstance {
    pub(crate) info: SpvInfoCell,
    pub(crate) clients: HashMap<u8, SpvClientCell>,
}

impl SpvInfoCell {
    pub(crate) fn prev_tip_client_id(&self) -> u8 {
        let current = self.info.tip_client_id;
        if current == 0 {
            self.clients_count - 1
        } else {
            current - 1
        }
    }

    pub(crate) fn next_tip_client_id(&self) -> u8 {
        let next = self.info.tip_client_id + 1;
        if next < self.clients_count {
            next
        } else {
            0
        }
    }
}

pub trait CkbRpcClientExtension {
    fn send_transaction_ext(&self, tx_json: TransactionView, dry_run: bool) -> Result<H256>;
    fn find_raw_spv_cells(&self, spv_type_script: Script) -> Result<Vec<LiveCell>>;

    fn find_spv_cells(&self, spv_type_script: Script) -> Result<SpvInstance> {
        let cells = self.find_raw_spv_cells(spv_type_script)?;
        parse_raw_spv_cells(cells)
    }

    fn find_best_spv_client(
        &self,
        spv_type_script: Script,
        height_opt: Option<u32>,
    ) -> Result<SpvClientCell> {
        let SpvInstance { mut info, clients } = self.find_spv_cells(spv_type_script)?;
        if let Some(height) = height_opt {
            for _ in 0..clients.len() {
                let cell = clients.get(&info.info.tip_client_id).ok_or_else(|| {
                    let msg = format!(
                        "the SPV client (id={}) is not found",
                        info.info.tip_client_id
                    );
                    Error::other(msg)
                })?;
                if cell.client.headers_mmr_root.max_height <= height {
                    return Ok(cell.to_owned());
                }
                info.info.tip_client_id = info.prev_tip_client_id();
            }
            let msg =
                format!("all SPV clients have better heights than server has (height: {height})");
            Err(Error::other(msg))
        } else {
            clients
                .get(&info.info.tip_client_id)
                .ok_or_else(|| {
                    let msg = format!(
                        "the SPV client (id={}) is not found",
                        info.info.tip_client_id
                    );
                    Error::other(msg)
                })
                .cloned()
        }
    }
}

impl CkbRpcClientExtension for CkbRpcClient {
    fn send_transaction_ext(&self, tx_json: TransactionView, dry_run: bool) -> Result<H256> {
        if log::log_enabled!(log::Level::Trace) {
            match serde_json::to_string_pretty(&tx_json) {
                Ok(tx_json_str) => {
                    log::trace!("transaction: {tx_json_str}")
                }
                Err(err) => {
                    log::warn!("failed to convert the transaction into json string since {err}")
                }
            }
        }

        let tx: Transaction = tx_json.inner.clone().into();
        let tx_hash = tx.calc_tx_hash().unpack();

        if log::log_enabled!(log::Level::Debug) {
            let cycles: u64 = self.estimate_cycles(tx_json.inner.clone())?.cycles.into();
            log::debug!("Estimated cycles for {tx_hash:#x}: {cycles}");
        }

        if !dry_run {
            let tx_hash = self.send_transaction(tx_json.inner, None)?;
            log::info!("Transaction hash: {tx_hash:#x}");
            println!("Send transaction: {tx_hash:#x}");
        }

        Ok(tx_hash)
    }

    fn find_raw_spv_cells(&self, spv_type_script: Script) -> Result<Vec<LiveCell>> {
        let args_data = spv_type_script.args().raw_data();
        let args = packed::SpvTypeArgsReader::from_slice(&args_data)
            .map_err(|err| {
                let msg = format!("the args of the SPV type script is invalid since {err}");
                Error::other(msg)
            })?
            .unpack();

        let query = CellQueryOptions::new(spv_type_script, PrimaryScriptType::Type);
        let order = Order::Desc;
        let search_key = SearchKey::from(query);

        self.get_cells(search_key, order, u32::MAX.into(), None)
            .map_err(Into::into)
            .map(|res| res.objects)
            .and_then(|cells| {
                let actual = cells.len();
                let expected = usize::from(args.clients_count) + 1;
                if actual == expected {
                    Ok(cells.into_iter().map(Into::into).collect())
                } else {
                    let msg = format!(
                        "the count of SPV cells is incorrect, expect {expected} but got {actual}"
                    );
                    Err(Error::other(msg))
                }
            })
    }
}

fn parse_raw_spv_cells(cells: Vec<LiveCell>) -> Result<SpvInstance> {
    let mut spv_info_opt = None;
    let mut spv_clients = HashMap::new();
    let clients_count = (cells.len() - 1) as u8; // Checked when fetch SPV cells.
    for cell in cells.into_iter() {
        let data = &cell.output_data;
        if let Ok(client) = packed::SpvClientReader::from_slice(data) {
            let spv_cell = SpvClientCell {
                client: client.unpack(),
                cell,
            };
            spv_clients.insert(spv_cell.client.id, spv_cell);
        } else if let Ok(info) = packed::SpvInfoReader::from_slice(data) {
            if spv_info_opt.is_some() {
                let msg = "the SPV info cell should be unique";
                return Err(Error::other(msg));
            }
            let spv_cell = SpvInfoCell {
                info: info.unpack(),
                cell,
                clients_count,
            };
            spv_info_opt = Some(spv_cell);
        } else {
            let msg = "the data of the SPV cell is unexpected";
            return Err(Error::other(msg));
        }
    }
    if let Some(spv_info) = spv_info_opt {
        let instance = SpvInstance {
            info: spv_info,
            clients: spv_clients,
        };
        Ok(instance)
    } else {
        let msg = "the SPV info cell is missing";
        Err(Error::other(msg))
    }
}
