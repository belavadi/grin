// Copyright 2016 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{fmt, num, thread, time};
use std::convert::From;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::path::MAIN_SEPARATOR;
use std::collections::HashMap;

use serde_json;
use secp;

use api;
use core::core::{Transaction, transaction};
use core::ser;
use keychain;
use util;
use util::LOGGER;

const DAT_FILE: &'static str = "wallet.dat";
const LOCK_FILE: &'static str = "wallet.lock";

const DEFAULT_BASE_FEE: u64 = 10;

/// Transaction fee calculation
pub fn tx_fee(input_len: usize, output_len: usize, base_fee: Option<u64>) -> u64 {
	let use_base_fee = match base_fee {
		Some(bf) => bf,
		None => DEFAULT_BASE_FEE,
	};
	let mut tx_weight = -1 * (input_len as i32) + 4 * (output_len as i32) + 1;
	if tx_weight < 1 {
		tx_weight = 1;
	}

	(tx_weight as u64) * use_base_fee
}

/// Wallet errors, mostly wrappers around underlying crypto or I/O errors.
#[derive(Debug)]
pub enum Error {
	NotEnoughFunds(u64),
	FeeDispute{sender_fee: u64, recipient_fee: u64},
	Keychain(keychain::Error),
	Transaction(transaction::Error),
	Secp(secp::Error),
	WalletData(String),
	/// An error in the format of the JSON structures exchanged by the wallet
	Format(String),
	/// Error when contacting a node through its API
	Node(api::Error),
}

impl From<keychain::Error> for Error {
	fn from(e: keychain::Error) -> Error {
		Error::Keychain(e)
	}
}

impl From<secp::Error> for Error {
	fn from(e: secp::Error) -> Error {
		Error::Secp(e)
	}
}

impl From<transaction::Error> for Error {
	fn from(e: transaction::Error) -> Error {
		Error::Transaction(e)
	}
}

impl From<serde_json::Error> for Error {
	fn from(e: serde_json::Error) -> Error {
		Error::Format(e.to_string())
	}
}

impl From<num::ParseIntError> for Error {
	fn from(_: num::ParseIntError) -> Error {
		Error::Format("Invalid hex".to_string())
	}
}

impl From<api::Error> for Error {
	fn from(e: api::Error) -> Error {
		Error::Node(e)
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletConfig {
	// Whether to run a wallet
	pub enable_wallet: bool,
	// The api address that this api server (i.e. this wallet) will run
	pub api_http_addr: String,
	// The api address of a running server node, against which transaction inputs will be checked
	// during send
	pub check_node_api_http_addr: String,
	// The directory in which wallet files are stored
	pub data_file_dir: String,
}

impl Default for WalletConfig {
	fn default() -> WalletConfig {
		WalletConfig {
			enable_wallet: false,
			api_http_addr: "127.0.0.1:13416".to_string(),
			check_node_api_http_addr: "http://127.0.0.1:13413".to_string(),
			data_file_dir: ".".to_string(),
		}
	}
}

/// Status of an output that's being tracked by the wallet. Can either be
/// unconfirmed, spent, unspent, or locked (when it's been used to generate
/// a transaction but we don't have confirmation that the transaction was
/// broadcasted or mined).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum OutputStatus {
	Unconfirmed,
	Unspent,
	Immature,
	Locked,
	Spent,
}

impl fmt::Display for OutputStatus {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match *self {
			OutputStatus::Unconfirmed => write!(f, "Unconfirmed"),
			OutputStatus::Unspent => write!(f, "Unspent"),
			OutputStatus::Immature => write!(f, "Immature"),
			OutputStatus::Locked => write!(f, "Locked"),
			OutputStatus::Spent => write!(f, "Spent"),
		}
	}
}

/// Information about an output that's being tracked by the wallet. Must be
/// enough to reconstruct the commitment associated with the ouput when the
/// root private key is known.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OutputData {
	/// Root key_id that the key for this output is derived from
	pub root_key_id: keychain::Identifier,
	/// Derived key for this output
	pub key_id: keychain::Identifier,
	/// How many derivations down from the root key
	pub n_child: u32,
	/// Value of the output, necessary to rebuild the commitment
	pub value: u64,
	/// Current status of the output
	pub status: OutputStatus,
	/// Height of the output
	pub height: u64,
	/// Height we are locked until
	pub lock_height: u64,
	/// Can we spend with zero confirmations? (Did it originate from us, change output etc.)
	pub zero_ok: bool,
}

impl OutputData {
	/// Lock a given output to avoid conflicting use
	fn lock(&mut self) {
		self.status = OutputStatus::Locked;
	}
}

/// Wallet information tracking all our outputs. Based on HD derivation and
/// avoids storing any key data, only storing output amounts and child index.
/// This data structure is directly based on the JSON representation stored
/// on disk, so selection algorithms are fairly primitive and non optimized.
///
/// TODO optimization so everything isn't O(n) or even O(n^2)
/// TODO account for fees
/// TODO write locks so files don't get overwritten
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WalletData {
	pub outputs: HashMap<String, OutputData>,
}

impl WalletData {
	/// Allows the reading and writing of the wallet data within a file lock.
	/// Just provide a closure taking a mutable WalletData. The lock should
	/// be held for as short a period as possible to avoid contention.
	/// Note that due to the impossibility to do an actual file lock easily
	/// across operating systems, this just creates a lock file with a "should
	/// not exist" option.
	pub fn with_wallet<T, F>(data_file_dir: &str, f: F) -> Result<T, Error>
		where F: FnOnce(&mut WalletData) -> T
	{
		// create directory if it doesn't exist
		fs::create_dir_all(data_file_dir).unwrap_or_else(|why| {
			info!(LOGGER, "! {:?}", why.kind());
		});

		let data_file_path = &format!("{}{}{}", data_file_dir, MAIN_SEPARATOR, DAT_FILE);
		let lock_file_path = &format!("{}{}{}", data_file_dir, MAIN_SEPARATOR, LOCK_FILE);

		// create the lock files, if it already exists, will produce an error
		// sleep and retry a few times if we cannot get it the first time
		let mut retries = 0;
		loop {
			let result = OpenOptions::new()
				.write(true)
				.create_new(true)
				.open(lock_file_path)
				.map_err(|_| {
					Error::WalletData(format!(
						"Could not create wallet lock file. Either \
					some other process is using the wallet or there's a write access issue."
					))
				});
			match result {
				Ok(_) => {
					break;
				}
				Err(e) => {
					if retries >= 3 {
						return Err(e);
					}
					debug!(
						LOGGER,
						"failed to obtain wallet.lock, retries - {}, sleeping",
						retries
					);
					retries += 1;
					thread::sleep(time::Duration::from_millis(500));
				}
			}
		}


		// do what needs to be done
		let mut wdat = WalletData::read_or_create(data_file_path)?;
		let res = f(&mut wdat);
		wdat.write(data_file_path)?;

		// delete the lock file
		fs::remove_file(lock_file_path).map_err(|_| {
			Error::WalletData(format!(
				"Could not remove wallet lock file. Maybe insufficient rights?"
			))
		})?;

		Ok(res)
	}

	/// Read the wallet data or created a brand new one if it doesn't exist yet
	fn read_or_create(data_file_path: &str) -> Result<WalletData, Error> {
		if Path::new(data_file_path).exists() {
			WalletData::read(data_file_path)
		} else {
			// just create a new instance, it will get written afterward
			Ok(WalletData { outputs: HashMap::new() })
		}
	}

	/// Read the wallet data from disk.
	fn read(data_file_path: &str) -> Result<WalletData, Error> {
		let data_file =
			File::open(data_file_path)
				.map_err(|e| Error::WalletData(format!("Could not open {}: {}", data_file_path, e)))?;
		serde_json::from_reader(data_file)
			.map_err(|e| Error::WalletData(format!("Error reading {}: {}", data_file_path, e)))
	}

	/// Write the wallet data to disk.
	fn write(&self, data_file_path: &str) -> Result<(), Error> {
		let mut data_file =
			File::create(data_file_path)
				.map_err(|e| {
					Error::WalletData(format!("Could not create {}: {}", data_file_path, e))
				})?;
		let res_json = serde_json::to_vec_pretty(self)
			.map_err(|e| Error::WalletData(format!("Error serializing wallet data: {}", e)))?;
		data_file
			.write_all(res_json.as_slice())
			.map_err(|e| Error::WalletData(format!("Error writing {}: {}", data_file_path, e)))
	}

	/// Append a new output data to the wallet data.
	/// TODO - we should check for overwriting here - only really valid for
	/// unconfirmed coinbase
	pub fn add_output(&mut self, out: OutputData) {
		self.outputs.insert(out.key_id.to_hex(), out.clone());
	}

	/// Lock an output data.
	/// TODO - we should track identifier on these outputs (not just n_child)
	pub fn lock_output(&mut self, out: &OutputData) {
		if let Some(out_to_lock) = self.outputs.get_mut(&out.key_id.to_hex()) {
			if out_to_lock.value == out.value {
				out_to_lock.lock()
			}
		}
	}

	pub fn get_output(&self, key_id: &keychain::Identifier) -> Option<&OutputData> {
		self.outputs.get(&key_id.to_hex())
	}

	/// Select a subset of unspent outputs to spend in a transaction
	/// transferring the provided amount.
	pub fn select(&self, root_key_id: keychain::Identifier, amount: u64) -> (Vec<OutputData>, i64) {
		let mut to_spend = vec![];
		let mut input_total = 0;

		for out in self.outputs.values() {
			if out.root_key_id == root_key_id
				&& (out.status == OutputStatus::Unspent)
					// the following will let us spend zero confirmation change outputs
					// || (out.status == OutputStatus::Unconfirmed && out.zero_ok))
			{
				to_spend.push(out.clone());
				input_total += out.value;
				if input_total >= amount {
					break;
				}
			}
		}
		// TODO - clean up our handling of i64 vs u64 so we are consistent
		(to_spend, (input_total as i64) - (amount as i64))
	}

	/// Next child index when we want to create a new output.
	pub fn next_child(&self, root_key_id: keychain::Identifier) -> u32 {
		let mut max_n = 0;
		for out in self.outputs.values() {
			if max_n < out.n_child && out.root_key_id == root_key_id {
				max_n = out.n_child;
			}
		}
		max_n + 1
	}
}

/// Helper in serializing the information a receiver requires to build a
/// transaction.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct JSONPartialTx {
	amount: u64,
	blind_sum: String,
	tx: String,
}

/// Encodes the information for a partial transaction (not yet completed by the
/// receiver) into JSON.
pub fn partial_tx_to_json(receive_amount: u64,
                          blind_sum: keychain::BlindingFactor,
                          tx: Transaction)
                          -> String {
	let partial_tx = JSONPartialTx {
		amount: receive_amount,
		blind_sum: util::to_hex(blind_sum.secret_key().as_ref().to_vec()),
		tx: util::to_hex(ser::ser_vec(&tx).unwrap()),
	};
	serde_json::to_string_pretty(&partial_tx).unwrap()
}

/// Reads a partial transaction encoded as JSON into the amount, sum of blinding
/// factors and the transaction itself.
pub fn partial_tx_from_json(keychain: &keychain::Keychain,
                            json_str: &str)
                            -> Result<(u64, keychain::BlindingFactor, Transaction), Error> {
	let partial_tx: JSONPartialTx = serde_json::from_str(json_str)?;

	let blind_bin = util::from_hex(partial_tx.blind_sum)?;

	// TODO - turn some data into a blinding factor here somehow
	// let blinding = SecretKey::from_slice(&secp, &blind_bin[..])?;
	let blinding = keychain::BlindingFactor::from_slice(keychain.secp(), &blind_bin[..])?;

	let tx_bin = util::from_hex(partial_tx.tx)?;
	let tx = ser::deserialize(&mut &tx_bin[..])
		.map_err(|_| {
			Error::Format("Could not deserialize transaction, invalid format.".to_string())
		})?;

	Ok((partial_tx.amount, blinding, tx))
}

/// Amount in request to build a coinbase output.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum WalletReceiveRequest {
	Coinbase(BlockFees),
	PartialTransaction(String),
	Finalize(String),
}

/// Fees in block to use for coinbase amount calculation
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BlockFees {
	pub fees: u64,
	pub height: u64,
	pub key_id: Option<keychain::Identifier>,
}

impl BlockFees {
	pub fn key_id(&self) -> Option<keychain::Identifier> {
		self.key_id.clone()
	}
}

/// Response to build a coinbase output.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CbData {
	pub output: String,
	pub kernel: String,
	pub key_id: String,
}
