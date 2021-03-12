// Copyright 2019 The Grin Developers
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

//! Selection of inputs for building transactions

use crate::address;
use crate::error::{Error, ErrorKind};
use crate::grin_core::core::amount_to_hr_string;
use crate::grin_core::core::transaction::TokenKey;
use crate::grin_core::libtx::{
	build,
	proof::{ProofBuild, ProofBuilder},
	tx_fee, DEFAULT_BASE_FEE,
};
use crate::grin_keychain::{Identifier, Keychain};
use crate::grin_util::secp::key::{SecretKey, ZERO_KEY};
use crate::grin_util::secp::pedersen;
use crate::internal::keys;
use crate::slate::Slate;
use crate::types::*;
use crate::util::OnionV3Address;
use std::collections::HashMap;

/// Initialize a transaction on the sender side, returns a corresponding
/// libwallet transaction slate with the appropriate inputs selected,
/// and saves the private wallet identifiers of our selected outputs
/// into our transaction context

pub fn build_send_tx<'a, T: ?Sized, C, K>(
	wallet: &mut T,
	keychain: &K,
	keychain_mask: Option<&SecretKey>,
	slate: &mut Slate,
	current_height: u64,
	minimum_confirmations: u64,
	max_outputs: usize,
	change_outputs: usize,
	selection_strategy_is_use_all: bool,
	parent_key_id: Identifier,
	is_invoice: bool,
	use_test_nonce: bool,
) -> Result<Context, Error>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
{
	//TODO: Revise HF3. If we're sending V4 slates, only include
	// change outputs in excess sum
	let include_inputs_in_sum = !slate.is_compact();

	let (elems, inputs, change_amounts_derivations, fee) = select_send_tx(
		wallet,
		keychain_mask,
		slate.amount,
		current_height,
		minimum_confirmations,
		max_outputs,
		change_outputs,
		selection_strategy_is_use_all,
		&parent_key_id,
		0,
		0,
		include_inputs_in_sum,
	)?;

	// Update the fee on the slate so we account for this when building the tx.
	slate.fee = fee;

	let (blinding, _) =
		slate.add_transaction_elements(keychain, &ProofBuilder::new(keychain), elems)?;

	// Create our own private context
	let mut context = Context::new(
		keychain.secp(),
		blinding.secret_key(&keychain.secp()).unwrap(),
		ZERO_KEY,
		&parent_key_id,
		use_test_nonce,
		is_invoice,
	);

	context.fee = fee;
	context.amount = slate.amount;

	// Store our private identifiers for each input
	for input in inputs {
		context.add_input(&input.key_id, &input.mmr_index, input.value);
	}

	let mut commits: HashMap<Identifier, Option<String>> = HashMap::new();

	// Store change output(s) and cached commits
	for (change_amount, id, mmr_index) in &change_amounts_derivations {
		context.add_output(&id, &mmr_index, *change_amount);
		commits.insert(
			id.clone(),
			wallet.calc_commit_for_cache(keychain_mask, *change_amount, &id)?,
		);
	}

	Ok(context)
}

pub fn build_send_token_tx<'a, T: ?Sized, C, K>(
	wallet: &mut T,
	keychain: &K,
	keychain_mask: Option<&SecretKey>,
	slate: &mut Slate,
	current_height: u64,
	minimum_confirmations: u64,
	max_outputs: usize,
	change_outputs: usize,
	selection_strategy_is_use_all: bool,
	parent_key_id: Identifier,
	is_invoice: bool,
	use_test_nonce: bool,
) -> Result<Context, Error>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
{
	if slate.token_type.is_none() {
		return Err(ErrorKind::GenericError(
			"token type should not be none".to_owned(),
		))?;
	}

	//TODO: Revise HF3. If we're sending V4 slates, only include
	// change outputs in excess sum
	let include_inputs_in_sum = !slate.is_compact();

	let (mut token_elems, token_inputs, token_change_amounts_derivations) = select_send_token_tx(
		wallet,
		keychain_mask,
		slate.amount,
		slate.token_type.clone().unwrap().as_str(),
		current_height,
		minimum_confirmations,
		max_outputs,
		change_outputs,
		selection_strategy_is_use_all,
		&parent_key_id,
		true,
	)?;

	let token_output_len = token_change_amounts_derivations.len() + 1;
	let token_inout_len = token_elems.len() - token_change_amounts_derivations.len();

	let (mut elems, inputs, change_amounts_derivations, fee) = select_send_tx(
		wallet,
		keychain_mask,
		0,
		current_height,
		minimum_confirmations,
		max_outputs,
		1,
		selection_strategy_is_use_all,
		&parent_key_id,
		token_inout_len,
		token_output_len,
		include_inputs_in_sum,
	)?;

	let mut all_elems = vec![];
	all_elems.append(&mut token_elems);
	all_elems.append(&mut elems);

	slate.fee = fee;
	let (blinding, token_blinding) =
		slate.add_transaction_elements(keychain, &ProofBuilder::new(keychain), all_elems)?;

	// Create our own private context
	let mut context = Context::new(
		keychain.secp(),
		blinding.secret_key(&keychain.secp()).unwrap(),
		token_blinding.secret_key(&keychain.secp()).unwrap(),
		&parent_key_id,
		use_test_nonce,
		is_invoice,
	);

	context.fee = fee;
	context.amount = slate.amount;

	// Store our private identifiers for each input
	for input in inputs {
		context.add_input(&input.key_id, &input.mmr_index, input.value);
	}

	// Store change output(s) and cached commits
	for (change_amount, id, mmr_index) in &change_amounts_derivations {
		context.add_output(&id, &mmr_index, *change_amount);
	}

	// Store our private identifiers for each token input
	for input in token_inputs {
		context.add_token_input(&input.key_id, &input.mmr_index, input.value);
	}

	// Store change token output(s) and cached commits
	for (change_amount, id, mmr_index) in &token_change_amounts_derivations {
		context.add_token_output(&id, &mmr_index, *change_amount);
	}

	Ok(context)
}

/// Locks all corresponding outputs in the context, creates
/// change outputs and tx log entry
pub fn lock_tx_context<'a, T: ?Sized, C, K>(
	wallet: &mut T,
	keychain_mask: Option<&SecretKey>,
	slate: &Slate,
	current_height: u64,
	context: &Context,
	excess_override: Option<pedersen::Commitment>,
) -> Result<(), Error>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
{
	let mut output_commits: HashMap<Identifier, (Option<String>, u64)> = HashMap::new();
	// Store cached commits before locking wallet
	let mut total_change = 0;
	for (id, _, change_amount) in &context.get_outputs() {
		output_commits.insert(
			id.clone(),
			(
				wallet.calc_commit_for_cache(keychain_mask, *change_amount, &id)?,
				*change_amount,
			),
		);
		total_change += change_amount;
	}

	debug!("Vcash Change amount is: {}", total_change);

	let keychain = wallet.keychain(keychain_mask)?;

	if slate.token_type.is_some() {
		let mut token_output_commits: HashMap<Identifier, (Option<String>, u64)> = HashMap::new();
		// Store cached commits before locking wallet
		let mut token_total_change = 0;
		for (id, _, change_amount) in &context.get_token_outputs() {
			token_output_commits.insert(
				id.clone(),
				(
					wallet.calc_commit_for_cache(keychain_mask, *change_amount, &id)?,
					*change_amount,
				),
			);
			token_total_change += change_amount;
		}

		debug!("Token Change amount is: {}", token_total_change);

		let lock_inputs = context.get_inputs().clone();
		let lock_token_inputs = context.get_token_inputs().clone();
		let slate_id = slate.id;
		let height = current_height;
		let parent_key_id = context.parent_key_id.clone();
		let mut batch = wallet.batch(keychain_mask)?;
		let log_id = batch.next_tx_log_id(&parent_key_id)?;
		let token_tx_type = if lock_token_inputs.len() == 0 {
			TokenTxLogEntryType::TokenIssue
		} else {
			TokenTxLogEntryType::TokenTxSent
		};
		let mut t = TokenTxLogEntry::new(parent_key_id.clone(), token_tx_type.clone(), log_id);
		t.tx_slate_id = Some(slate_id);
		t.token_type = slate.token_type.clone().unwrap();
		let filename = format!("{}.vcashtx", slate_id);
		t.stored_tx = Some(filename);
		t.fee = Some(context.fee);
		t.ttl_cutoff_height = match slate.ttl_cutoff_height {
			0 => None,
			n => Some(n),
		};

		if let Ok(e) = slate.calc_excess(keychain.secp()) {
			t.kernel_excess = Some(e)
		}
		if let Some(e) = excess_override {
			t.kernel_excess = Some(e)
		}
		t.kernel_lookup_min_height = Some(current_height);

		let mut amount_debited = 0;
		t.num_inputs = lock_inputs.len();
		for id in lock_inputs {
			let mut coin = batch.get(&id.0, &id.1).unwrap();
			coin.tx_log_entry = Some(log_id);
			amount_debited += coin.value;
			batch.lock_output(&mut coin)?;
		}
		t.amount_debited = amount_debited;

		let mut token_amount_debited = 0;
		t.num_token_inputs = lock_token_inputs.len();
		for id in lock_token_inputs {
			let mut coin = batch.get_token(&id.0, &id.1).unwrap();
			coin.tx_log_entry = Some(log_id);
			token_amount_debited += coin.value;
			batch.lock_token_output(&mut coin)?;
		}
		t.token_amount_debited = token_amount_debited;

		// store extra payment proof info, if required
		if let Some(ref p) = slate.payment_proof {
			let sender_address_path = match context.payment_proof_derivation_index {
				Some(p) => p,
				None => {
					return Err(ErrorKind::PaymentProof(
						"Payment proof derivation index required".to_owned(),
					)
					.into());
				}
			};
			let sender_key = address::address_from_derivation_path(
				&keychain,
				&parent_key_id,
				sender_address_path,
			)?;
			let sender_address = OnionV3Address::from_private(&sender_key.0)?;
			t.payment_proof = Some(StoredProofInfo {
				receiver_address: p.receiver_address,
				receiver_signature: p.receiver_signature,
				sender_address: sender_address.to_ed25519()?,
				sender_address_path,
				sender_signature: None,
			});
		};

		// write the output representing our change
		for (id, _, _) in &context.get_outputs() {
			t.num_outputs += 1;
			let (commit, change_amount) = output_commits.get(&id).unwrap().clone();
			t.amount_credited += change_amount;
			batch.save(OutputData {
				root_key_id: parent_key_id.clone(),
				key_id: id.clone(),
				n_child: id.to_path().last_path_index(),
				commit: commit,
				mmr_index: None,
				value: change_amount.clone(),
				status: OutputStatus::Unconfirmed,
				height: height,
				lock_height: 0,
				is_coinbase: false,
				tx_log_entry: Some(log_id),
			})?;
		}

		// write the token output representing our change
		for (id, _, _) in &context.get_token_outputs() {
			t.num_token_outputs += 1;
			let (commit, change_amount) = token_output_commits.get(&id).unwrap().clone();
			t.token_amount_credited += change_amount;
			batch.save_token(TokenOutputData {
				root_key_id: parent_key_id.clone(),
				key_id: id.clone(),
				n_child: id.to_path().last_path_index(),
				commit: commit,
				token_type: slate.token_type.clone().unwrap(),
				mmr_index: None,
				value: change_amount,
				status: OutputStatus::Unconfirmed,
				height: height,
				lock_height: 0,
				is_token_issue: (token_tx_type == TokenTxLogEntryType::TokenIssue),
				tx_log_entry: Some(log_id),
			})?;
		}
		batch.save_token_tx_log_entry(t.clone(), &parent_key_id)?;
		batch.commit()?;
	} else {
		let lock_inputs = context.get_inputs().clone();
		let slate_id = slate.id;
		let height = current_height;
		let parent_key_id = context.parent_key_id.clone();
		let mut batch = wallet.batch(keychain_mask)?;
		let log_id = batch.next_tx_log_id(&parent_key_id)?;
		let mut t = TxLogEntry::new(parent_key_id.clone(), TxLogEntryType::TxSent, log_id);
		t.tx_slate_id = Some(slate_id);
		let filename = format!("{}.vcashtx", slate_id);
		t.stored_tx = Some(filename);
		t.fee = Some(context.fee);
		t.ttl_cutoff_height = match slate.ttl_cutoff_height {
			0 => None,
			n => Some(n),
		};

		if let Ok(e) = slate.calc_excess(keychain.secp()) {
			t.kernel_excess = Some(e)
		}
		if let Some(e) = excess_override {
			t.kernel_excess = Some(e)
		}
		t.kernel_lookup_min_height = Some(current_height);

		let mut amount_debited = 0;
		t.num_inputs = lock_inputs.len();
		for id in lock_inputs {
			let mut coin = batch.get(&id.0, &id.1).unwrap();
			coin.tx_log_entry = Some(log_id);
			amount_debited += coin.value;
			batch.lock_output(&mut coin)?;
		}

		t.amount_debited = amount_debited;

		// store extra payment proof info, if required
		if let Some(ref p) = slate.payment_proof {
			let sender_address_path = match context.payment_proof_derivation_index {
				Some(p) => p,
				None => {
					return Err(ErrorKind::PaymentProof(
						"Payment proof derivation index required".to_owned(),
					)
					.into());
				}
			};
			let sender_key = address::address_from_derivation_path(
				&keychain,
				&parent_key_id,
				sender_address_path,
			)?;
			let sender_address = OnionV3Address::from_private(&sender_key.0)?;
			t.payment_proof = Some(StoredProofInfo {
				receiver_address: p.receiver_address,
				receiver_signature: p.receiver_signature,
				sender_address: sender_address.to_ed25519()?,
				sender_address_path,
				sender_signature: None,
			});
		};

		// write the output representing our change
		for (id, _, _) in &context.get_outputs() {
			t.num_outputs += 1;
			let (commit, change_amount) = output_commits.get(&id).unwrap().clone();
			t.amount_credited += change_amount;
			batch.save(OutputData {
				root_key_id: parent_key_id.clone(),
				key_id: id.clone(),
				n_child: id.to_path().last_path_index(),
				commit: commit,
				mmr_index: None,
				value: change_amount,
				status: OutputStatus::Unconfirmed,
				height: height,
				lock_height: 0,
				is_coinbase: false,
				tx_log_entry: Some(log_id),
			})?;
		}
		batch.save_tx_log_entry(t.clone(), &parent_key_id)?;
		batch.commit()?;
	}

	wallet.store_tx(&format!("{}", slate.id), slate.tx_or_err()?)?;
	Ok(())
}

/// Creates a new output in the wallet for the recipient,
/// returning the key of the fresh output
/// Also creates a new transaction containing the output
pub fn build_recipient_output<'a, T: ?Sized, C, K>(
	wallet: &mut T,
	keychain_mask: Option<&SecretKey>,
	slate: &mut Slate,
	current_height: u64,
	parent_key_id: Identifier,
	is_invoice: bool,
	use_test_rng: bool,
) -> Result<
	(
		Identifier,
		Context,
		Option<TxLogEntry>,
		Option<TokenTxLogEntry>,
	),
	Error,
>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
{
	// Create a potential output for this transaction
	let key_id = keys::next_available_key(wallet, keychain_mask).unwrap();
	let keychain = wallet.keychain(keychain_mask)?;
	let key_id_inner = key_id.clone();
	let amount = slate.amount;
	let height = current_height;

	let slate_id = slate.id;
	let elem = if slate.token_type.clone().is_some() {
		vec![build::token_output(
			amount,
			TokenKey::from_hex(slate.token_type.clone().unwrap().as_str())?,
			false,
			key_id.clone(),
		)]
	} else {
		vec![build::output(amount, key_id.clone())]
	};
	let (blinding, token_blinding) =
		slate.add_transaction_elements(&keychain, &ProofBuilder::new(&keychain), elem)?;

	// Add blinding sum to our context
	let mut context = Context::new(
		keychain.secp(),
		blinding
			.secret_key(wallet.keychain(keychain_mask)?.secp())
			.unwrap(),
		token_blinding
			.secret_key(wallet.keychain(keychain_mask)?.secp())
			.unwrap(),
		&parent_key_id,
		use_test_rng,
		is_invoice,
	);

	if slate.token_type.clone().is_some() {
		context.add_token_output(&key_id, &None, amount);
	} else {
		context.add_output(&key_id, &None, amount);
	};
	context.amount = amount;
	context.fee = slate.fee;
	let commit = wallet.calc_commit_for_cache(keychain_mask, amount, &key_id_inner)?;
	let mut batch = wallet.batch(keychain_mask)?;
	let log_id = batch.next_tx_log_id(&parent_key_id)?;
	if slate.token_type.clone().is_some() {
		let mut t = TokenTxLogEntry::new(
			parent_key_id.clone(),
			TokenTxLogEntryType::TokenTxReceived,
			log_id,
		);
		t.tx_slate_id = Some(slate_id);
		t.token_type = slate.token_type.clone().unwrap();
		t.token_amount_credited = amount;
		t.num_token_outputs = 1;
		t.ttl_cutoff_height = match slate.ttl_cutoff_height {
			0 => None,
			n => Some(n),
		};
		// when invoicing, this will be invalid
		if let Ok(e) = slate.calc_excess(keychain.secp()) {
			t.kernel_excess = Some(e)
		}
		t.kernel_lookup_min_height = Some(current_height);
		batch.save_token(TokenOutputData {
			root_key_id: parent_key_id.clone(),
			key_id: key_id_inner.clone(),
			mmr_index: None,
			n_child: key_id_inner.to_path().last_path_index(),
			commit: commit,
			token_type: slate.token_type.clone().unwrap(),
			value: amount,
			status: OutputStatus::Unconfirmed,
			height: height,
			lock_height: 0,
			is_token_issue: false,
			tx_log_entry: Some(log_id),
		})?;
		batch.save_token_tx_log_entry(t.clone(), &parent_key_id)?;
		batch.commit()?;

		Ok((key_id, context, None, Some(t)))
	} else {
		let mut t = TxLogEntry::new(parent_key_id.clone(), TxLogEntryType::TxReceived, log_id);
		t.tx_slate_id = Some(slate_id);
		t.amount_credited = amount;
		t.num_outputs = 1;
		t.ttl_cutoff_height = match slate.ttl_cutoff_height {
			0 => None,
			n => Some(n),
		};
		// when invoicing, this will be invalid
		if let Ok(e) = slate.calc_excess(keychain.secp()) {
			t.kernel_excess = Some(e)
		}
		t.kernel_lookup_min_height = Some(current_height);
		batch.save(OutputData {
			root_key_id: parent_key_id.clone(),
			key_id: key_id_inner.clone(),
			mmr_index: None,
			n_child: key_id_inner.to_path().last_path_index(),
			commit: commit,
			value: amount,
			status: OutputStatus::Unconfirmed,
			height: height,
			lock_height: 0,
			is_coinbase: false,
			tx_log_entry: Some(log_id),
		})?;
		batch.save_tx_log_entry(t.clone(), &parent_key_id)?;
		batch.commit()?;

		Ok((key_id, context, Some(t), None))
	}
}

/// Builds a transaction to send to someone from the HD seed associated with the
/// wallet and the amount to send. Handles reading through the wallet data file,
/// selecting outputs to spend and building the change.
pub fn select_send_tx<'a, T: ?Sized, C, K, B>(
	wallet: &mut T,
	keychain_mask: Option<&SecretKey>,
	amount: u64,
	current_height: u64,
	minimum_confirmations: u64,
	max_outputs: usize,
	change_outputs: usize,
	selection_strategy_is_use_all: bool,
	parent_key_id: &Identifier,
	token_inputs: usize,
	token_outputs: usize,
	include_inputs_in_sum: bool,
) -> Result<
	(
		Vec<Box<build::Append<K, B>>>,
		Vec<OutputData>,
		Vec<(u64, Identifier, Option<u64>)>, // change amounts and derivations
		u64,                                 // fee
	),
	Error,
>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
	B: ProofBuild,
{
	let (coins, _total, amount, fee) = select_coins_and_fee(
		wallet,
		amount,
		current_height,
		minimum_confirmations,
		max_outputs,
		change_outputs,
		selection_strategy_is_use_all,
		&parent_key_id,
		token_inputs,
		token_outputs,
	)?;

	// build transaction skeleton with inputs and change
	let (parts, change_amounts_derivations) = inputs_and_change(
		&coins,
		wallet,
		keychain_mask,
		amount,
		fee,
		change_outputs,
		include_inputs_in_sum,
	)?;

	Ok((parts, coins, change_amounts_derivations, fee))
}

/// Builds a transaction to send to someone from the HD seed associated with the
/// wallet and the amount to send. Handles reading through the wallet data file,
/// selecting outputs to spend and building the change.
pub fn select_send_token_tx<'a, T: ?Sized, C, K, B>(
	wallet: &mut T,
	keychain_mask: Option<&SecretKey>,
	amount: u64,
	token_type: &str,
	current_height: u64,
	minimum_confirmations: u64,
	max_outputs: usize,
	change_outputs: usize,
	selection_strategy_is_use_all: bool,
	parent_key_id: &Identifier,
	include_inputs_in_sum: bool,
) -> Result<
	(
		Vec<Box<build::Append<K, B>>>,
		Vec<TokenOutputData>,
		Vec<(u64, Identifier, Option<u64>)>, // change amounts and derivations
	),
	Error,
>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
	B: ProofBuild,
{
	let (coins, _total, amount) = select_token_coins_and_fee(
		wallet,
		amount,
		token_type,
		current_height,
		minimum_confirmations,
		max_outputs,
		selection_strategy_is_use_all,
		&parent_key_id,
	)?;

	// build transaction skeleton with inputs and change
	let token_type = TokenKey::from_hex(&token_type)?;
	let (parts, change_amounts_derivations) = token_inputs_and_change(
		&coins,
		wallet,
		keychain_mask,
		amount,
		token_type,
		change_outputs,
		include_inputs_in_sum,
	)?;

	Ok((parts, coins, change_amounts_derivations))
}

/// Select outputs and calculating fee.
pub fn select_coins_and_fee<'a, T: ?Sized, C, K>(
	wallet: &mut T,
	amount: u64,
	current_height: u64,
	minimum_confirmations: u64,
	max_outputs: usize,
	change_outputs: usize,
	selection_strategy_is_use_all: bool,
	parent_key_id: &Identifier,
	token_inputs: usize,
	token_outputs: usize,
) -> Result<
	(
		Vec<OutputData>,
		u64, // total
		u64, // amount
		u64, // fee
	),
	Error,
>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
{
	let min_fee = DEFAULT_BASE_FEE;
	let amount_with_fee = amount + min_fee;

	// select some spendable coins from the wallet
	let (max_outputs, mut coins) = select_coins(
		wallet,
		amount_with_fee,
		current_height,
		minimum_confirmations,
		max_outputs,
		selection_strategy_is_use_all,
		parent_key_id,
	);

	// sender is responsible for setting the fee on the partial tx
	// recipient should double check the fee calculation and not blindly trust the
	// sender

	// TODO - Is it safe to spend without a change output? (1 input -> 1 output)
	// TODO - Does this not potentially reveal the senders private key?
	//

	// First attempt to spend without change
	let output_len = if amount == 0 { 0 } else { 1 };

	let token_kernel_len = if token_outputs == 0 { 0 } else { 1 };
	let mut fee = tx_fee(
		coins.len(),
		output_len,
		1,
		token_inputs,
		token_outputs,
		token_kernel_len,
		None,
	);
	let mut total: u64 = coins.iter().map(|c| c.value).sum();
	let mut amount_with_fee = amount + fee;

	if total == 0 {
		return Err(ErrorKind::NotEnoughFunds {
			available: 0,
			available_disp: amount_to_hr_string(0, false),
			needed: amount_with_fee as u64,
			needed_disp: amount_to_hr_string(amount_with_fee as u64, false),
		}
		.into());
	}

	// The amount with fee is more than the total values of our max outputs
	if total < amount_with_fee && coins.len() == max_outputs {
		return Err(ErrorKind::NotEnoughFunds {
			available: total,
			available_disp: amount_to_hr_string(total, false),
			needed: amount_with_fee as u64,
			needed_disp: amount_to_hr_string(amount_with_fee as u64, false),
		}
		.into());
	}

	let num_outputs = change_outputs + output_len;

	// We need to add a change address or amount with fee is more than total
	if total != amount_with_fee {
		fee = tx_fee(
			coins.len(),
			num_outputs,
			1,
			token_inputs,
			token_outputs,
			token_kernel_len,
			None,
		);
		amount_with_fee = amount + fee;

		// Here check if we have enough outputs for the amount including fee otherwise
		// look for other outputs and check again
		while total < amount_with_fee {
			// End the loop if we have selected all the outputs and still not enough funds
			if coins.len() == max_outputs {
				return Err(ErrorKind::NotEnoughFunds {
					available: total as u64,
					available_disp: amount_to_hr_string(total, false),
					needed: amount_with_fee as u64,
					needed_disp: amount_to_hr_string(amount_with_fee as u64, false),
				}
				.into());
			}

			// select some spendable coins from the wallet
			coins = select_coins(
				wallet,
				amount_with_fee,
				current_height,
				minimum_confirmations,
				max_outputs,
				selection_strategy_is_use_all,
				parent_key_id,
			)
			.1;
			fee = tx_fee(
				coins.len(),
				num_outputs,
				1,
				token_inputs,
				token_outputs,
				token_kernel_len,
				None,
			);
			total = coins.iter().map(|c| c.value).sum();
			amount_with_fee = amount + fee;
		}
	}
	Ok((coins, total, amount, fee))
}

/// Select outputs and calculating fee.
pub fn select_token_coins_and_fee<'a, T: ?Sized, C, K>(
	wallet: &mut T,
	amount: u64,
	token_type: &str,
	current_height: u64,
	minimum_confirmations: u64,
	max_outputs: usize,
	selection_strategy_is_use_all: bool,
	parent_key_id: &Identifier,
) -> Result<
	(
		Vec<TokenOutputData>,
		u64, // total
		u64, // amount
	),
	Error,
>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
{
	// select some spendable coins from the wallet
	let (max_outputs, coins) = select_token_coins(
		wallet,
		amount,
		token_type,
		current_height,
		minimum_confirmations,
		max_outputs,
		selection_strategy_is_use_all,
		parent_key_id,
	);

	let total: u64 = coins.iter().map(|c| c.value).sum();

	if total == 0 {
		return Err(ErrorKind::NotEnoughFunds {
			available: 0,
			available_disp: amount_to_hr_string(0, false),
			needed: amount as u64,
			needed_disp: amount_to_hr_string(amount as u64, false),
		})?;
	}

	// The amount with fee is more than the total values of our max outputs
	if total < amount && coins.len() == max_outputs {
		return Err(ErrorKind::NotEnoughFunds {
			available: total,
			available_disp: amount_to_hr_string(total, false),
			needed: amount as u64,
			needed_disp: amount_to_hr_string(amount as u64, false),
		})?;
	}

	Ok((coins, total, amount))
}

/// Selects inputs and change for a transaction
pub fn inputs_and_change<'a, T: ?Sized, C, K, B>(
	coins: &[OutputData],
	wallet: &mut T,
	keychain_mask: Option<&SecretKey>,
	amount: u64,
	fee: u64,
	num_change_outputs: usize,
	include_inputs_in_sum: bool,
) -> Result<
	(
		Vec<Box<build::Append<K, B>>>,
		Vec<(u64, Identifier, Option<u64>)>,
	),
	Error,
>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
	B: ProofBuild,
{
	let mut parts = vec![];

	// calculate the total across all inputs, and how much is left
	let total: u64 = coins.iter().map(|c| c.value).sum();

	// if we are spending 10,000 coins to send 1,000 then our change will be 9,000
	// if the fee is 80 then the recipient will receive 1000 and our change will be
	// 8,920
	let change = total - amount - fee;

	// build inputs using the appropriate derived key_ids
	if include_inputs_in_sum {
		for coin in coins {
			if coin.is_coinbase {
				parts.push(build::coinbase_input(coin.value, coin.key_id.clone()));
			} else {
				parts.push(build::input(coin.value, coin.key_id.clone()));
			}
		}
	}

	let mut change_amounts_derivations = vec![];

	if change == 0 {
		debug!("No change (sending exactly amount + fee), no change outputs to build");
	} else {
		debug!(
			"Building change outputs: total change: {} ({} outputs)",
			change, num_change_outputs
		);

		let part_change = change / num_change_outputs as u64;
		let remainder_change = change % part_change;

		for x in 0..num_change_outputs {
			// n-1 equal change_outputs and a final one accounting for any remainder
			let change_amount = if x == (num_change_outputs - 1) {
				part_change + remainder_change
			} else {
				part_change
			};

			let change_key = wallet.next_child(keychain_mask).unwrap();

			change_amounts_derivations.push((change_amount, change_key.clone(), None));
			parts.push(build::output(change_amount, change_key));
		}
	}

	Ok((parts, change_amounts_derivations))
}

/// Selects token inputs and change for a transaction
pub fn token_inputs_and_change<'a, T: ?Sized, C, K, B>(
	coins: &Vec<TokenOutputData>,
	wallet: &mut T,
	keychain_mask: Option<&SecretKey>,
	amount: u64,
	token_type: TokenKey,
	num_change_outputs: usize,
	include_inputs_in_sum: bool,
) -> Result<
	(
		Vec<Box<build::Append<K, B>>>,
		Vec<(u64, Identifier, Option<u64>)>,
	),
	Error,
>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
	B: ProofBuild,
{
	let mut parts = vec![];

	// calculate the total across all inputs, and how much is left
	let total: u64 = coins.iter().map(|c| c.value).sum();

	// if we are spending 10,000 coins to send 1,000 then our change will be 9,000
	// if the fee is 80 then the recipient will receive 1000 and our change will be
	// 8,920
	let change = total - amount;

	// build inputs using the appropriate derived key_ids
	if include_inputs_in_sum {
		for coin in coins {
			parts.push(build::build_token_input(
				coin.value,
				token_type,
				coin.is_token_issue,
				coin.key_id.clone(),
			));
		}
	}

	let mut change_amounts_derivations = vec![];

	if change == 0 {
		debug!("No Token change (sending exactly amount + fee), no change outputs to build");
	} else {
		debug!(
			"Building Token change outputs: total change: {} ({} outputs)",
			change, num_change_outputs
		);

		let part_change = change / num_change_outputs as u64;
		let remainder_change = change % part_change;

		for x in 0..num_change_outputs {
			// n-1 equal change_outputs and a final one accounting for any remainder
			let change_amount = if x == (num_change_outputs - 1) {
				part_change + remainder_change
			} else {
				part_change
			};

			let change_key = wallet.next_child(keychain_mask).unwrap();

			change_amounts_derivations.push((change_amount, change_key.clone(), None));
			parts.push(build::token_output(
				change_amount,
				token_type,
				false,
				change_key,
			));
		}
	}

	Ok((parts, change_amounts_derivations))
}

/// Select spendable coins from a wallet.
/// Default strategy is to spend the maximum number of outputs (up to
/// max_outputs). Alternative strategy is to spend smallest outputs first
/// but only as many as necessary. When we introduce additional strategies
/// we should pass something other than a bool in.
/// TODO: Possibly move this into another trait to be owned by a wallet?

pub fn select_coins<'a, T: ?Sized, C, K>(
	wallet: &mut T,
	amount: u64,
	current_height: u64,
	minimum_confirmations: u64,
	max_outputs: usize,
	select_all: bool,
	parent_key_id: &Identifier,
) -> (usize, Vec<OutputData>)
//    max_outputs_available, Outputs
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
{
	// first find all eligible outputs based on number of confirmations
	let mut eligible = wallet
		.iter()
		.filter(|out| {
			out.root_key_id == *parent_key_id
				&& out.eligible_to_spend(current_height, minimum_confirmations)
		})
		.collect::<Vec<OutputData>>();

	let max_available = eligible.len();

	// sort eligible outputs by increasing value
	eligible.sort_by_key(|out| out.value);

	// use a sliding window to identify potential sets of possible outputs to spend
	// Case of amount > total amount of max_outputs(500):
	// The limit exists because by default, we always select as many inputs as
	// possible in a transaction, to reduce both the Output set and the fees.
	// But that only makes sense up to a point, hence the limit to avoid being too
	// greedy. But if max_outputs(500) is actually not enough to cover the whole
	// amount, the wallet should allow going over it to satisfy what the user
	// wants to send. So the wallet considers max_outputs more of a soft limit.
	if eligible.len() > max_outputs {
		for window in eligible.windows(max_outputs) {
			let windowed_eligibles = window.to_vec();
			if let Some(outputs) = select_from(amount, select_all, windowed_eligibles) {
				return (max_available, outputs);
			}
		}
		// Not exist in any window of which total amount >= amount.
		// Then take coins from the smallest one up to the total amount of selected
		// coins = the amount.
		if let Some(outputs) = select_from(amount, false, eligible.clone()) {
			debug!(
				"Extending maximum number of outputs. {} outputs selected.",
				outputs.len()
			);
			return (max_available, outputs);
		}
	} else if let Some(outputs) = select_from(amount, select_all, eligible.clone()) {
		return (max_available, outputs);
	}

	// we failed to find a suitable set of outputs to spend,
	// so return the largest amount we can so we can provide guidance on what is
	// possible
	eligible.reverse();
	(
		max_available,
		eligible.iter().take(max_outputs).cloned().collect(),
	)
}

fn select_from(amount: u64, select_all: bool, outputs: Vec<OutputData>) -> Option<Vec<OutputData>> {
	let total = outputs.iter().fold(0, |acc, x| acc + x.value);
	if total >= amount {
		if select_all {
			Some(outputs.to_vec())
		} else {
			let mut selected_amount = 0;
			Some(
				outputs
					.iter()
					.take_while(|out| {
						let res = selected_amount < amount;
						selected_amount += out.value;
						res
					})
					.cloned()
					.collect(),
			)
		}
	} else {
		None
	}
}

pub fn build_issue_token_tx<'a, T: ?Sized, C, K>(
	wallet: &mut T,
	keychain: &K,
	keychain_mask: Option<&SecretKey>,
	slate: &mut Slate,
	current_height: u64,
	minimum_confirmations: u64,
	max_outputs: usize,
	change_outputs: usize,
	selection_strategy_is_use_all: bool,
	parent_key_id: Identifier,
	use_test_nonce: bool,
) -> Result<Context, Error>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
{
	let (mut elems, inputs, change_amounts_derivations, fee) = select_send_tx(
		wallet,
		keychain_mask,
		0,
		current_height,
		minimum_confirmations,
		max_outputs,
		change_outputs,
		selection_strategy_is_use_all,
		&parent_key_id,
		0,
		1,
		true,
	)?;

	let token_type = TokenKey::new_token_key();
	let (mut token_elems, (amount, key_id, mmr_index)) =
		token_issue_output(wallet, keychain_mask, slate.amount, token_type.clone())?;

	let mut all_elems = vec![];
	all_elems.append(&mut token_elems);
	all_elems.append(&mut elems);

	slate.fee = fee;
	let (blinding, token_blinding) =
		slate.add_transaction_elements(keychain, &ProofBuilder::new(keychain), all_elems)?;

	slate.token_type = Some(token_type.to_hex());
	slate.construct_issue_token_kernel(keychain, amount, &key_id)?;

	// Create our own private context
	let mut context = Context::new(
		keychain.secp(),
		blinding.secret_key(&keychain.secp()).unwrap(),
		token_blinding.secret_key(&keychain.secp()).unwrap(),
		&parent_key_id,
		use_test_nonce,
		false,
	);

	context.fee = fee;

	// Store our private identifiers for each input
	for input in inputs {
		context.add_input(&input.key_id, &input.mmr_index, input.value);
	}

	// Store change output(s) and cached commits
	for (change_amount, id, mmr_index) in &change_amounts_derivations {
		context.add_output(&id, &mmr_index, *change_amount);
	}

	// Store change token output(s) and cached commits
	context.add_token_output(&key_id, &mmr_index, amount);

	Ok(context)
}

/// Selects token inputs and change for a transaction
pub fn token_issue_output<'a, T: ?Sized, C, K, B>(
	wallet: &mut T,
	keychain_mask: Option<&SecretKey>,
	amount: u64,
	token_type: TokenKey,
) -> Result<
	(
		Vec<Box<build::Append<K, B>>>,
		(u64, Identifier, Option<u64>),
	),
	Error,
>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
	B: ProofBuild,
{
	let mut parts = vec![];

	let token_key = wallet.next_child(keychain_mask).unwrap();
	parts.push(build::token_output(
		amount,
		token_type,
		true,
		token_key.clone(),
	));

	Ok((parts, (amount, token_key.clone(), None)))
}

pub fn select_token_coins<'a, T: ?Sized, C, K>(
	wallet: &mut T,
	amount: u64,
	token_type: &str,
	current_height: u64,
	minimum_confirmations: u64,
	max_outputs: usize,
	select_all: bool,
	parent_key_id: &Identifier,
) -> (usize, Vec<TokenOutputData>)
//    max_outputs_available, Outputs
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
{
	// first find all eligible outputs based on number of confirmations
	let mut eligible = wallet
		.token_iter()
		.filter(|out| {
			out.root_key_id == *parent_key_id
				&& out.token_type == token_type
				&& out.eligible_to_spend(current_height, minimum_confirmations)
		})
		.collect::<Vec<TokenOutputData>>();

	let max_available = eligible.len();

	// sort eligible outputs by increasing value
	eligible.sort_by_key(|out| out.value);

	// use a sliding window to identify potential sets of possible outputs to spend
	// Case of amount > total amount of max_outputs(500):
	// The limit exists because by default, we always select as many inputs as
	// possible in a transaction, to reduce both the Output set and the fees.
	// But that only makes sense up to a point, hence the limit to avoid being too
	// greedy. But if max_outputs(500) is actually not enough to cover the whole
	// amount, the wallet should allow going over it to satisfy what the user
	// wants to send. So the wallet considers max_outputs more of a soft limit.
	if eligible.len() > max_outputs {
		for window in eligible.windows(max_outputs) {
			let windowed_eligibles = window.iter().cloned().collect::<Vec<_>>();
			if let Some(outputs) = select_token_from(amount, select_all, windowed_eligibles) {
				return (max_available, outputs);
			}
		}
		// Not exist in any window of which total amount >= amount.
		// Then take coins from the smallest one up to the total amount of selected
		// coins = the amount.
		if let Some(outputs) = select_token_from(amount, false, eligible.clone()) {
			debug!(
				"Extending maximum number of outputs. {} outputs selected.",
				outputs.len()
			);
			return (max_available, outputs);
		}
	} else {
		if let Some(outputs) = select_token_from(amount, select_all, eligible.clone()) {
			return (max_available, outputs);
		}
	}

	// we failed to find a suitable set of outputs to spend,
	// so return the largest amount we can so we can provide guidance on what is
	// possible
	eligible.reverse();
	(
		max_available,
		eligible.iter().take(max_outputs).cloned().collect(),
	)
}

fn select_token_from(
	amount: u64,
	select_all: bool,
	outputs: Vec<TokenOutputData>,
) -> Option<Vec<TokenOutputData>> {
	let total = outputs.iter().fold(0, |acc, x| acc + x.value);
	if total >= amount {
		if select_all {
			return Some(outputs.iter().cloned().collect());
		} else {
			let mut selected_amount = 0;
			return Some(
				outputs
					.iter()
					.take_while(|out| {
						let res = selected_amount < amount;
						selected_amount += out.value;
						res
					})
					.cloned()
					.collect(),
			);
		}
	} else {
		None
	}
}

/// Repopulates output in the slate's tranacstion
/// with outputs from the stored context
/// change outputs and tx log entry
/// Remove the explicitly stored excess
pub fn repopulate_tx<'a, T: ?Sized, C, K>(
	wallet: &mut T,
	keychain_mask: Option<&SecretKey>,
	slate: &mut Slate,
	context: &Context,
	update_fee: bool,
) -> Result<(), Error>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
{
	// restore the original amount, fee
	slate.amount = context.amount;
	if update_fee {
		slate.fee = context.fee;
	}

	let keychain = wallet.keychain(keychain_mask)?;

	// restore my signature data
	let key = match slate.token_type.clone() {
		Some(_) => &context.token_sec_key,
		None => &context.sec_key,
	};
	slate.add_participant_info(&keychain, key, &context.sec_nonce, None)?;

	let mut parts = vec![];
	for (id, _, value) in &context.get_inputs() {
		let input = wallet.iter().find(|out| out.key_id == *id);
		if let Some(i) = input {
			if i.is_coinbase {
				parts.push(build::coinbase_input(*value, i.key_id.clone()));
			} else {
				parts.push(build::input(*value, i.key_id.clone()));
			}
		}
	}
	for (id, _, value) in &context.get_outputs() {
		let output = wallet.iter().find(|out| out.key_id == *id);
		if let Some(i) = output {
			parts.push(build::output(*value, i.key_id.clone()));
		}
	}
	for (id, _, value) in &context.get_token_inputs() {
		let output = wallet.token_iter().find(|out| out.key_id == *id);
		if let Some(i) = output {
			parts.push(build::token_input(
				*value,
				TokenKey::from_hex(slate.token_type.clone().unwrap().as_str())?,
				i.is_token_issue,
				i.key_id.clone(),
			));
		}
	}
	for (id, _, value) in &context.get_token_outputs() {
		let output = wallet.token_iter().find(|out| out.key_id == *id);
		if let Some(i) = output {
			parts.push(build::token_output(
				*value,
				TokenKey::from_hex(slate.token_type.clone().unwrap().as_str())?,
				false,
				i.key_id.clone(),
			));
		}
	}
	let _ = slate.add_transaction_elements(&keychain, &ProofBuilder::new(&keychain), parts)?;
	// restore the original offset
	slate.tx_or_err_mut()?.offset = slate.offset.clone();
	Ok(())
}
