use std::cmp::{max, min};
use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::ops::BitXor;
use std::sync::{Arc, Weak};
use std::time::Duration;

use crate::block_reward_hbbft::BlockRewardContract;
use client_traits::{EngineClient, ForceUpdateSealing};
use common_types::{
	engines::{params::CommonParams, Seal, SealingState},
	errors::{BlockError, EngineError, EthcoreError as Error},
	header::Header,
	ids::BlockId,
	transaction::SignedTransaction,
	BlockNumber,
};
use engine::{signer::EngineSigner, Engine};
use ethereum_types::{H256, H512, U256};
use ethjson::spec::HbbftParams;
use hbbft::Target;
use io::{IoContext, IoHandler, IoService, TimerToken};
use itertools::Itertools;
use machine::{ExecutedBlock, Machine};
use parity_crypto::publickey::Signature;
use parking_lot::RwLock;
use rlp::{self, Decodable, Rlp};
use serde::Deserialize;
use serde_json;

use crate::contracts::keygen_history::{initialize_synckeygen, send_keygen_transactions};
use crate::contracts::validator_set::{
	get_pending_validators, is_pending_validator, ValidatorType,
};
use crate::contribution::{unix_now_millis, unix_now_secs};
use crate::hbbft_state::{Batch, HbMessage, HbbftState, HoneyBadgerStep};
use crate::sealing::{self, RlpSig, Sealing};
use crate::NodeId;

type TargetedMessage = hbbft::TargetedMessage<Message, NodeId>;

/// A message sent between validators that is part of Honey Badger BFT or the block sealing process.
#[derive(Debug, Deserialize, Serialize)]
enum Message {
	/// A Honey Badger BFT message.
	HoneyBadger(usize, HbMessage),
	/// A threshold signature share. The combined signature is used as the block seal.
	Sealing(BlockNumber, sealing::Message),
}

pub struct HoneyBadgerBFT {
	transition_service: IoService<()>,
	client: Arc<RwLock<Option<Weak<dyn EngineClient>>>>,
	signer: Arc<RwLock<Option<Box<dyn EngineSigner>>>>,
	machine: Machine,
	hbbft_state: RwLock<HbbftState>,
	sealing: RwLock<BTreeMap<BlockNumber, Sealing>>,
	params: HbbftParams,
	message_counter: RwLock<usize>,
	random_numbers: RwLock<BTreeMap<BlockNumber, U256>>,
}

struct TransitionHandler {
	client: Arc<RwLock<Option<Weak<dyn EngineClient>>>>,
	engine: Arc<HoneyBadgerBFT>,
}

const DEFAULT_DURATION: Duration = Duration::from_secs(1);

impl TransitionHandler {
	/// Returns the approximate time duration between the latest block and the minimum block time
	/// or a keep-alive time duration of 1s.
	fn duration_remaining_since_last_block(&self, client: Arc<dyn EngineClient>) -> Duration {
		if let Some(block_header) = client.block_header(BlockId::Latest) {
			// The block timestamp and minimum block time are specified in seconds.
			let next_block_time =
				(block_header.timestamp() + self.engine.params.minimum_block_time) as u128 * 1000;

			// We get the current time in milliseconds to calculate the exact timer duration.
			let now = unix_now_millis();

			if now >= next_block_time {
				// If the current time is already past the minimum time for the next block,
				// just return the 1s keep alive interval.
				DEFAULT_DURATION
			} else {
				// Otherwise wait the exact number of milliseconds needed for the
				// now >= next_block_time condition to be true.
				// Since we know that "now" is smaller than "next_block_time" at this point
				// we also know that "next_block_time - now" will always be a positive number.
				match u64::try_from(next_block_time - now) {
					Ok(value) => Duration::from_millis(value),
					_ => {
						error!(target: "consensus", "Could not convert duration to next block to u64");
						DEFAULT_DURATION
					}
				}
			}
		} else {
			error!(target: "consensus", "Latest Block Header could not be obtained!");
			DEFAULT_DURATION
		}
	}
}

// Arbitrary identifier for the timer we register with the event handler.
const ENGINE_TIMEOUT_TOKEN: TimerToken = 1;

impl IoHandler<()> for TransitionHandler {
	fn initialize(&self, io: &IoContext<()>) {
		// Start the event loop with an arbitrary timer
		io.register_timer_once(ENGINE_TIMEOUT_TOKEN, DEFAULT_DURATION)
			.unwrap_or_else(
				|e| warn!(target: "consensus", "Failed to start consensus timer: {}.", e),
			)
	}

	fn timeout(&self, io: &IoContext<()>, timer: TimerToken) {
		if timer == ENGINE_TIMEOUT_TOKEN {
			trace!(target: "consensus", "Honey Badger IoHandler timeout called");
			// The block may be complete, but not have been ready to seal - trigger a new seal attempt.
			// TODO: In theory, that should not happen. The seal is ready exactly when the sealing entry is `Complete`.
			if let Some(ref weak) = *self.client.read() {
				if let Some(c) = weak.upgrade() {
					c.update_sealing(ForceUpdateSealing::No);
				}
			}

			// Transactions may have been submitted during creation of the last block, trigger the
			// creation of a new block if the transaction threshold has been reached.
			self.engine.on_transactions_imported();

			// The client may not be registered yet on startup, we set the default duration.
			let mut timer_duration = DEFAULT_DURATION;
			if let Some(ref weak) = *self.client.read() {
				if let Some(c) = weak.upgrade() {
					timer_duration = self.duration_remaining_since_last_block(c);
					// The duration should be at least 1ms and at most self.engine.params.minimum_block_time
					timer_duration = max(timer_duration, Duration::from_millis(1));
					timer_duration = min(
						timer_duration,
						Duration::from_secs(self.engine.params.minimum_block_time),
					);
				}
			}

			io.register_timer_once(ENGINE_TIMEOUT_TOKEN, timer_duration)
				.unwrap_or_else(
					|e| warn!(target: "consensus", "Failed to restart consensus step timer: {}.", e),
				);
		}
	}
}

impl HoneyBadgerBFT {
	pub fn new(params: HbbftParams, machine: Machine) -> Result<Arc<dyn Engine>, Box<Error>> {
		let engine = Arc::new(HoneyBadgerBFT {
			transition_service: IoService::<()>::start().map_err(|err| Box::new(err.into()))?,
			client: Arc::new(RwLock::new(None)),
			signer: Arc::new(RwLock::new(None)),
			machine,
			hbbft_state: RwLock::new(HbbftState::new()),
			sealing: RwLock::new(BTreeMap::new()),
			params,
			message_counter: RwLock::new(0),
			random_numbers: RwLock::new(BTreeMap::new()),
		});

		if !engine.params.is_unit_test.unwrap_or(false) {
			let handler = TransitionHandler {
				client: engine.client.clone(),
				engine: engine.clone(),
			};
			engine
				.transition_service
				.register_handler(Arc::new(handler))
				.map_err(|err| Box::new(err.into()))?;
		}

		Ok(engine)
	}

	fn process_output(&self, client: Arc<dyn EngineClient>, output: Vec<Batch>) {
		// TODO: Multiple outputs are possible,
		//       process all outputs, respecting their epoch context.
		if output.len() > 1 {
			error!(target: "consensus", "UNHANDLED EPOCH OUTPUTS!");
		}
		let batch = match output.first() {
			None => return,
			Some(batch) => batch,
		};

		// Decode and de-duplicate transactions
		let batch_txns: Vec<_> = batch
			.contributions
			.iter()
			.flat_map(|(_, c)| &c.transactions)
			.filter_map(|ser_txn| {
				// TODO: Report proposers of malformed transactions.
				Decodable::decode(&Rlp::new(ser_txn)).ok()
			})
			.unique()
			.filter_map(|txn| {
				// TODO: Report proposers of invalidly signed transactions.
				SignedTransaction::new(txn).ok()
			})
			.collect();

		// We use the median of all contributions' timestamps
		let mut timestamps = batch
			.contributions
			.iter()
			.map(|(_, c)| c.timestamp)
			.sorted();

		let timestamp = match timestamps.nth(timestamps.len() / 2) {
			Some(t) => t,
			None => {
				error!(target: "consensus", "Error calculating the block timestamp");
				return;
			}
		};

		let random_number = batch
			.contributions
			.iter()
			.fold(U256::zero(), |acc, (n, c)| {
				if c.random_data.len() >= 32 {
					U256::from(&c.random_data[0..32]).bitxor(acc)
				} else {
					// TODO: Report malicious behavior by node!
					error!(target: "consensus", "Insufficient random data from node {}", n);
					acc
				}
			});

		self.random_numbers
			.write()
			.insert(batch.epoch, random_number);

		if let Some(header) = client.create_pending_block_at(batch_txns, timestamp, batch.epoch) {
			let block_num = header.number();
			let hash = header.bare_hash();
			trace!(target: "consensus", "Sending signature share of {} for block {}", hash, block_num);
			let step = match self
				.sealing
				.write()
				.entry(block_num)
				.or_insert_with(|| self.new_sealing())
				.sign(hash)
			{
				Ok(step) => step,
				Err(err) => {
					// TODO: Error handling
					error!(target: "consensus", "Error creating signature share for block {}: {:?}", block_num, err);
					return;
				}
			};
			self.process_seal_step(client, step, block_num);
		} else {
			error!(target: "consensus", "Could not create pending block for hbbft epoch {}: ", batch.epoch);
		}
	}

	fn process_hb_message(
		&self,
		msg_idx: usize,
		message: HbMessage,
		sender_id: NodeId,
	) -> Result<(), EngineError> {
		let client = self.client_arc().ok_or(EngineError::RequiresClient)?;
		trace!(target: "consensus", "Received message of idx {}  {:?} from {}", msg_idx, message, sender_id);
		let step = self
			.hbbft_state
			.write()
			.process_message(client.clone(), sender_id, message);

		if let Some(step) = step {
			self.process_step(client, step);
			self.join_hbbft_epoch()?;
		}
		Ok(())
	}

	fn process_sealing_message(
		&self,
		message: sealing::Message,
		sender_id: NodeId,
		block_num: BlockNumber,
	) -> Result<(), EngineError> {
		let client = self.client_arc().ok_or(EngineError::RequiresClient)?;
		trace!(target: "consensus", "Received sealing message  {:?} from {}", message, sender_id);
		if let Some(latest) = client.block_number(BlockId::Latest) {
			if latest >= block_num {
				return Ok(()); // Message is obsolete.
			}
		}

		trace!(target: "consensus", "Received signature share for block {} from {}", block_num, sender_id);
		let step_result = self
			.sealing
			.write()
			.entry(block_num)
			.or_insert_with(|| self.new_sealing())
			.handle_message(&sender_id, message);
		match step_result {
			Ok(step) => self.process_seal_step(client, step, block_num),
			Err(err) => error!(target: "consensus", "Error on ThresholdSign step: {:?}", err), // TODO: Errors
		}
		Ok(())
	}

	fn dispatch_messages<I>(&self, client: &Arc<dyn EngineClient>, messages: I)
	where
		I: IntoIterator<Item = TargetedMessage>,
	{
		for m in messages {
			let ser =
				serde_json::to_vec(&m.message).expect("Serialization of consensus message failed");
			let net_info = {
				let opt_net_info = self.hbbft_state.read();
				opt_net_info
					.network_info
					.as_ref()
					.expect("Network Info expected to be initialized")
					.clone()
			};
			match m.target {
				Target::Nodes(set) => {
					trace!(target: "consensus", "Dispatching message {:?} to {:?}", m.message, set);
					for node_id in set.into_iter().filter(|p| p != net_info.our_id()) {
						trace!(target: "consensus", "Sending message to {}", node_id.0);
						client.send_consensus_message(ser.clone(), Some(node_id.0));
					}
				}
				Target::AllExcept(set) => {
					trace!(target: "consensus", "Dispatching exclusive message {:?} to all except {:?}", m.message, set);
					for node_id in net_info
						.all_ids()
						.filter(|p| (p != &net_info.our_id() && !set.contains(p)))
					{
						trace!(target: "consensus", "Sending exclusive message to {}", node_id.0);
						client.send_consensus_message(ser.clone(), Some(node_id.0));
					}
				}
			}
		}
	}

	fn process_seal_step(
		&self,
		client: Arc<dyn EngineClient>,
		step: sealing::Step,
		block_num: BlockNumber,
	) {
		let messages = step
			.messages
			.into_iter()
			.map(|msg| msg.map(|m| Message::Sealing(block_num, m)));
		self.dispatch_messages(&client, messages);
		if let Some(sig) = step.output.into_iter().next() {
			trace!(target: "consensus", "Signature for block {} is ready", block_num);
			let state = Sealing::Complete(sig);
			self.sealing.write().insert(block_num, state);
			client.update_sealing(ForceUpdateSealing::No);
		}
	}

	fn process_step(&self, client: Arc<dyn EngineClient>, step: HoneyBadgerStep) {
		let mut message_counter = self.message_counter.write();
		let messages = step.messages.into_iter().map(|msg| {
			*message_counter += 1;
			TargetedMessage {
				target: msg.target,
				message: Message::HoneyBadger(*message_counter, msg.message),
			}
		});
		self.dispatch_messages(&client, messages);
		self.process_output(client, step.output);
	}

	/// Conditionally joins the current hbbft epoch if the number of received
	/// contributions exceeds the maximum number of tolerated faulty nodes.
	fn join_hbbft_epoch(&self) -> Result<(), EngineError> {
		let client = self.client_arc().ok_or(EngineError::RequiresClient)?;
		let step = self
			.hbbft_state
			.write()
			.contribute_if_contribution_threshold_reached(client.clone());
		if let Some(step) = step {
			self.process_step(client, step)
		}
		Ok(())
	}

	fn start_hbbft_epoch(&self, client: Arc<dyn EngineClient>) {
		let step = self
			.hbbft_state
			.write()
			.try_send_contribution(client.clone());
		if let Some(step) = step {
			self.process_step(client, step)
		}
	}

	fn transaction_queue_and_time_thresholds_reached(
		&self,
		client: &Arc<dyn EngineClient>,
	) -> bool {
		if let Some(block_header) = client.block_header(BlockId::Latest) {
			let target_min_timestamp = block_header.timestamp() + self.params.minimum_block_time;
			let now = unix_now_secs();
			let queue_length = client.queued_transactions().len();
			target_min_timestamp <= now
				&& queue_length >= self.params.transaction_queue_size_trigger
		} else {
			false
		}
	}

	fn new_sealing(&self) -> Sealing {
		let netinfo = self
			.hbbft_state
			.read()
			.network_info
			.as_ref()
			.expect("NetworkInfo not found")
			.clone();
		Sealing::new(netinfo)
	}

	fn client_arc(&self) -> Option<Arc<dyn EngineClient>> {
		self.client.read().as_ref().and_then(Weak::upgrade)
	}

	/// Returns true if we are in the keygen phase and a new key has been generated.
	fn do_keygen(&self) -> bool {
		match self.client_arc() {
			None => false,
			Some(client) => {
				// If we are not in key generation phase, return false.
				match get_pending_validators(&*client) {
					Err(_) => return false,
					Ok(validators) => {
						// If the validator set is empty then we are not in the key generation phase.
						if validators.is_empty() {
							return false;
						}
					}
				}

				// Check if a new key is ready to be generated, return true to switch to the new epoch in that case.
				if let Ok(synckeygen) = initialize_synckeygen(
					&*client,
					&self.signer,
					BlockId::Latest,
					ValidatorType::Pending,
				) {
					if synckeygen.is_ready() {
						return true;
					}
				}

				// Otherwise check if we are in the pending validator set and send Parts and Acks transactions.
				if let Some(signer) = self.signer.read().as_ref() {
					if let Ok(is_pending) = is_pending_validator(&*client, &signer.address()) {
						if is_pending {
							let _err = send_keygen_transactions(&*client, &self.signer);
						}
					}
				}
				false
			}
		}
	}

	fn check_for_epoch_change(&self) -> Option<()> {
		let client = self.client_arc()?;
		if let None = self
			.hbbft_state
			.write()
			.update_honeybadger(client, &self.signer, false)
		{
			info!(target: "consensus", "Fatal: Updating Honey Badger instance failed!");
		}
		Some(())
	}
}

impl Engine for HoneyBadgerBFT {
	fn name(&self) -> &str {
		"HoneyBadgerBFT"
	}

	fn machine(&self) -> &Machine {
		&self.machine
	}

	fn verify_local_seal(&self, _header: &Header) -> Result<(), Error> {
		self.check_for_epoch_change();
		Ok(())
	}

	/// Phase 1 Checks
	fn verify_block_basic(&self, _header: &Header) -> Result<(), Error> {
		Ok(())
	}

	/// Pase 2 Checks
	fn verify_block_unordered(&self, _header: &Header) -> Result<(), Error> {
		Ok(())
	}

	/// Phase 3 Checks
	/// We check the signature here since at this point the blocks are imported in-order.
	/// To verify the signature we need the parent block already imported on the chain.
	fn verify_block_family(&self, header: &Header, _parent: &Header) -> Result<(), Error> {
		self.check_for_epoch_change();

		let latest_block_nr = match self.client_arc() {
			Some(client) => client.block_number(BlockId::Latest).expect("must succeed"),
			None => 0,
		};

		if header.number() > (latest_block_nr + 1) {
			error!(target: "engine", "Phase 3 block verification out of order!");
			return Err(BlockError::InvalidSeal.into());
		}

		if header.seal().len() != 1 {
			return Err(BlockError::InvalidSeal.into());
		}
		let RlpSig(sig) = rlp::decode(header.seal().first().ok_or(BlockError::InvalidSeal)?)?;
		if self
			.hbbft_state
			.read()
			.public_master_key
			.expect("Missing public master key")
			.verify(&sig, header.bare_hash())
		{
			Ok(())
		} else {
			Err(BlockError::InvalidSeal.into())
		}
	}

	// Phase 4
	fn verify_block_external(&self, _header: &Header) -> Result<(), Error> {
		Ok(())
	}

	fn register_client(&self, client: Weak<dyn EngineClient>) {
		*self.client.write() = Some(client.clone());
		if let Some(client) = self.client_arc() {
			if let None = self
				.hbbft_state
				.write()
				.update_honeybadger(client, &self.signer, true)
			{
				// As long as the client is set we should be able to initialize as a regular node.
				error!(target: "engine", "Error during HoneyBadger initialization!");
			}
		}
	}

	fn set_signer(&self, signer: Option<Box<dyn EngineSigner>>) {
		*self.signer.write() = signer;
		if let Some(client) = self.client_arc() {
			if let None = self
				.hbbft_state
				.write()
				.update_honeybadger(client, &self.signer, true)
			{
				info!(target: "engine", "HoneyBadger Algorithm could not be created, Client possibly not set yet.");
			}
		}
	}

	fn sign(&self, hash: H256) -> Result<Signature, Error> {
		match self.signer.read().as_ref() {
			Some(signer) => signer
				.sign(hash)
				.map_err(|_| Error::Engine(EngineError::RequiresSigner)),
			None => Err(Error::Engine(EngineError::RequiresSigner)),
		}
	}

	fn generate_engine_transactions(
		&self,
		block: &ExecutedBlock,
	) -> Result<Vec<SignedTransaction>, Error> {
		self.check_for_epoch_change();
		let _random_number = match self.random_numbers.read().get(&block.header.number()) {
			None => {
				return Err(Error::Engine(EngineError::Custom(
					"No value available for calling randomness contract.".into(),
				)))
			}
			Some(r) => r,
		};
		Ok(Vec::new())
	}

	fn sealing_state(&self) -> SealingState {
		// Purge obsolete sealing processes.
		let client = match self.client_arc() {
			None => return SealingState::NotReady,
			Some(client) => client,
		};
		let next_block = match client.block_number(BlockId::Latest) {
			None => return SealingState::NotReady,
			Some(block_num) => block_num + 1,
		};
		let mut sealing = self.sealing.write();
		*sealing = sealing.split_off(&next_block);

		// We are ready to seal if we have a valid signature for the next block.
		if let Some(next_seal) = sealing.get(&next_block) {
			if next_seal.signature().is_some() {
				return SealingState::Ready;
			}
		}
		SealingState::NotReady
	}

	fn on_transactions_imported(&self) {
		self.check_for_epoch_change();
		if let Some(client) = self.client_arc() {
			if self.transaction_queue_and_time_thresholds_reached(&client) {
				self.start_hbbft_epoch(client);
			}
		}
	}

	fn handle_message(&self, message: &[u8], node_id: Option<H512>) -> Result<(), EngineError> {
		self.check_for_epoch_change();
		let node_id = NodeId(node_id.ok_or(EngineError::UnexpectedMessage)?);
		match serde_json::from_slice(message) {
			Ok(Message::HoneyBadger(msg_idx, hb_msg)) => {
				self.process_hb_message(msg_idx, hb_msg, node_id)
			}
			Ok(Message::Sealing(block_num, seal_msg)) => {
				self.process_sealing_message(seal_msg, node_id, block_num)
			}
			Err(_) => Err(EngineError::MalformedMessage(
				"Serde message decoding failed.".into(),
			)),
		}
	}

	fn seal_fields(&self, _header: &Header) -> usize {
		1
	}

	fn generate_seal(&self, block: &ExecutedBlock, _parent: &Header) -> Seal {
		let block_num = block.header.number();
		let sealing = self.sealing.read();
		let sig = match sealing.get(&block_num).and_then(Sealing::signature) {
			None => return Seal::None,
			Some(sig) => sig,
		};
		if !self
			.hbbft_state
			.read()
			.public_master_key
			.expect("Missing public master key")
			.verify(sig, block.header.bare_hash())
		{
			error!(target: "consensus", "Threshold signature does not match new block.");
			return Seal::None;
		}
		trace!(target: "consensus", "Returning seal for block {}.", block_num);
		Seal::Regular(vec![rlp::encode(&RlpSig(sig))])
	}

	fn should_miner_prepare_blocks(&self) -> bool {
		false
	}

	fn params(&self) -> &CommonParams {
		self.machine.params()
	}

	fn use_block_author(&self) -> bool {
		false
	}

	fn on_close_block(&self, block: &mut ExecutedBlock, _parent: &Header) -> Result<(), Error> {
		self.check_for_epoch_change();
		if let Some(address) = self.params.block_reward_contract_address {
			let mut call = engine::default_system_or_code_call(&self.machine, block);
			let contract = BlockRewardContract::new_from_address(address);
			let _total_reward = contract.reward(&mut call, self.do_keygen())?;
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use crate::contribution::Contribution;
	use crate::utils::test_helpers::create_transaction;
	use common_types::transaction::SignedTransaction;
	use ethereum_types::U256;
	use hbbft::honey_badger::{HoneyBadger, HoneyBadgerBuilder};
	use hbbft::NetworkInfo;
	use parity_crypto::publickey::{Generator, Random};
	use rand;
	use std::sync::Arc;

	#[test]
	fn test_single_contribution() {
		let mut rng = rand::thread_rng();
		let net_infos = NetworkInfo::generate_map(0..1usize, &mut rng)
			.expect("NetworkInfo generation is expected to always succeed");

		let net_info = net_infos
			.get(&0)
			.expect("A NetworkInfo must exist for node 0");

		let mut builder: HoneyBadgerBuilder<Contribution, _> =
			HoneyBadger::builder(Arc::new(net_info.clone()));

		let mut honey_badger = builder.build();

		let mut pending: Vec<SignedTransaction> = Vec::new();
		let keypair = Random.generate();
		pending.push(create_transaction(&keypair, &U256::from(1)));
		let input_contribution = Contribution::new(&pending);

		let step = honey_badger
			.propose(&input_contribution, &mut rng)
			.expect("Since there is only one validator we expect an immediate result");

		// Assure the contribution returned by HoneyBadger matches the input
		assert_eq!(step.output.len(), 1);
		let out = step.output.first().unwrap();
		assert_eq!(out.epoch, 0);
		assert_eq!(out.contributions.len(), 1);
		assert_eq!(out.contributions.get(&0).unwrap(), &input_contribution);
	}
}
