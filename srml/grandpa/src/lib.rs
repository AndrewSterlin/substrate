// Copyright 2017-2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! GRANDPA Consensus module for runtime.
//!
//! This manages the GRANDPA authority set ready for the native code.
//! These authorities are only for GRANDPA finality, not for consensus overall.
//!
//! In the future, it will also handle misbehavior reports, and on-chain
//! finality notifications.
//!
//! For full integration with GRANDPA, the `GrandpaApi` should be implemented.
//! The necessary items are re-exported via the `fg_primitives` crate.

#![cfg_attr(not(feature = "std"), no_std)]

// re-export since this is necessary for `impl_apis` in runtime.
pub use substrate_finality_grandpa_primitives as fg_primitives;

use rstd::prelude::*;
use parity_codec::{self as codec, Encode, Decode, Codec};

use srml_support::{
	decl_event, decl_storage, decl_module, dispatch::Result,
	traits::{KeyOwnerProofSystem}, Parameter,
	storage::{StorageValue, StorageMap, EnumerableStorageMap}
};
use primitives::{
	generic::{DigestItem, OpaqueDigestItemId, Block}, key_types,
	traits::{CurrentHeight, Verify, Header as HeaderT, Block as BlockT, Member, TypedKey},
};
use fg_primitives::{
	ScheduledChange, GRANDPA_ENGINE_ID, GrandpaPrecommit,
	SignedPrecommit, VoterSet, localized_payload, AncestryChain,
	Chain, validate_commit, ConsensusLog, equivocation::GrandpaEquivocation
};
pub use fg_primitives::{
	AuthorityId, AuthorityWeight, AuthoritySignature, CHALLENGE_SESSION_LENGTH,
	Commit, safety::{self, ChallengedVote},
};

use substrate_primitives::crypto::KeyTypeId;
use session::historical::Proof;
use system::{DigestOf, ensure_signed};
use core::iter::FromIterator;

mod mock;
mod tests;

type Hash<T> = <T as system::Trait>::Hash;
type Number<T> = <T as system::Trait>::BlockNumber;
type Header<T> = <T as system::Trait>::Header;
type Signature<T> = <T as Trait>::Signature;
type AuthorityIdOf<T> = <T as Trait>::AuthorityId;
type ProofOf<T> = <T as Trait>::Proof;

type StoredPendingChallenge<T> = safety::StoredPendingChallenge<Hash<T>, Number<T>, Header<T>, ProofOf<T>>;
type StoredChallengeSession<T> = safety::StoredChallengeSession<Hash<T>, Number<T>>;

// type Prevote<T> = GrandpaPrevote<Hash<T>, Number<T>>;
type Precommit<T> = GrandpaPrecommit<Hash<T>, Number<T>>;
// type Message<T> = GrandpaMessage<Hash<T>, Number<T>>;

type Equivocation<T> = GrandpaEquivocation<Hash<T>, Number<T>, Signature<T>, AuthorityIdOf<T>, ProofOf<T>>;
type Challenge<T> = safety::Challenge<Hash<T>, Number<T>, Header<T>, ProofOf<T>>;

pub trait Trait: system::Trait {
	/// The event type of this module.
	type Event: From<Event> + Into<<Self as system::Trait>::Event>;

	/// The identifier type for an authority.
	type AuthorityId: Codec + TypedKey + Default + Member;

	/// The signature of the authority.
	type Signature: Verify<Signer=Self::AuthorityId> + Codec + Member;

	/// The block.
	type Block: BlockT<Hash=<Self as system::Trait>::Hash, Header=<Self as system::Trait>::Header>;

	type Proof: Codec + Member;

	/// The session key proof owned system.
	type KeyOwnerSystem: KeyOwnerProofSystem<(KeyTypeId, Vec<u8>), Proof=Self::Proof>;
}

/// A stored pending change, old format.
// TODO: remove shim
// https://github.com/paritytech/substrate/issues/1614
#[derive(Encode, Decode)]
pub struct OldStoredPendingChange<N> {
	/// The block number this was scheduled at.
	pub scheduled_at: N,
	/// The delay in blocks until it will be applied.
	pub delay: N,
	/// The next authority set.
	pub next_authorities: Vec<(AuthorityId, u64)>,
}

/// A stored pending change.
#[derive(Encode)]
pub struct StoredPendingChange<N> {
	/// The block number this was scheduled at.
	pub scheduled_at: N,
	/// The delay in blocks until it will be applied.
	pub delay: N,
	/// The next authority set.
	pub next_authorities: Vec<(AuthorityId, u64)>,
	/// If defined it means the change was forced and the given block number
	/// indicates the median last finalized block when the change was signaled.
	pub forced: Option<N>,
}

impl<N: Decode> Decode for StoredPendingChange<N> {
	fn decode<I: codec::Input>(value: &mut I) -> Option<Self> {
		let old = OldStoredPendingChange::decode(value)?;
		let forced = <Option<N>>::decode(value).unwrap_or(None);

		Some(StoredPendingChange {
			scheduled_at: old.scheduled_at,
			delay: old.delay,
			next_authorities: old.next_authorities,
			forced,
		})
	}
}

/// Current state of the GRANDPA authority set. State transitions must happen in
/// the same order of states defined below, e.g. `Paused` implies a prior
/// `PendingPause`.
#[derive(Decode, Encode)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub enum StoredState<N> {
	/// The current authority set is live, and GRANDPA is enabled.
	Live,
	/// There is a pending pause event which will be enacted at the given block
	/// height.
	PendingPause {
		/// Block at which the intention to pause was scheduled.
		scheduled_at: N,
		/// Number of blocks after which the change will be enacted.
		delay: N
	},
	/// The current GRANDPA authority set is paused.
	Paused,
	/// There is a pending resume event which will be enacted at the given block
	/// height.
	PendingResume {
		/// Block at which the intention to resume was scheduled.
		scheduled_at: N,
		/// Number of blocks after which the change will be enacted.
		delay: N,
	},
}

decl_event!(
	pub enum Event {
		/// New authority set has been applied.
		NewAuthorities(Vec<(AuthorityId, u64)>),
		/// Current authority set has been paused.
		Paused,
		/// Current authority set has been resumed.
		Resumed,
		NewChallenge(Vec<AuthorityId>),
		ChallengeResponded(Vec<AuthorityId>),
	}
);

decl_storage! {
	trait Store for Module<T: Trait> as GrandpaFinality {
		/// The current authority set.
		Authorities get(authorities) config(): Vec<(AuthorityId, AuthorityWeight)>;

		/// State of the current authority set.
		State get(state): StoredState<T::BlockNumber> = StoredState::Live;

		/// Pending change: (signaled at, scheduled change).
		PendingChange: Option<StoredPendingChange<T::BlockNumber>>;

		/// A window of previous (closed) challenge sessions.
		HistoricalChallengeSessions get(historical_challenge_sessions): map T::Hash => Option<()>;

		/// Open challenge sessions.
		ChallengeSessions get(challenge_sessions): linked_map T::Hash => Option<StoredChallengeSession<T>>;

		/// Pending challenges.
		PendingChallenges get(pending_challenges): Vec<StoredPendingChallenge<T>>;

		/// next block number where we can force a change.
		NextForced get(next_forced): Option<T::BlockNumber>;

		/// `true` if we are currently stalled.
		Stalled get(stalled): Option<(T::BlockNumber, T::BlockNumber)>;
	}
}

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		fn deposit_event() = default;

		/// Report equivocation.
		fn report_equivocation(origin, equivocation: Equivocation<T>) {
			ensure_signed(origin)?;

			if equivocation.identity_proof.is_none() {
				return Err("Equivocation does not have inclusion proof")
			}

			let to_punish = <T as Trait>::KeyOwnerSystem::check_proof(
				(key_types::ED25519, equivocation.identity.encode()),
				equivocation.identity_proof
					.clone()
					.expect("already checked; qed")
			);

			if to_punish.is_none() {
				return Err("Bad session key proof")
			}

			if equivocation.is_valid() {
				// Slash
			}
		}

		/// Report rejecting set of prevotes.
		fn report_rejecting_set(origin, challenge: Challenge<T>) {
			ensure_signed(origin)?;

			let mut to_punish = Vec::new();

			if challenge.targets_proof.is_none() {
				return Err("Challenge does not have proof of inclusion")
			}

			for (idx, proof) in challenge.targets_proof
				.clone()
				.expect("already checked; qed")
				.iter()
				.enumerate() {
					let maybe_targets = <T as Trait>::KeyOwnerSystem::check_proof(
						(key_types::ED25519, challenge.targets[idx].encode()),
						proof.clone(),
					);
					if maybe_targets.is_none() {
						return Err("Bad session key proof")
					}
					to_punish.push(maybe_targets.expect("already checked; qed"));
			}

			let round_s = challenge.rejecting_set.round;
			let round_b = challenge.finalized_block_proof.round;

			if round_s == round_b {
				// Check that block proof contains supermajority for B.
				// TODO: Check signatures.
				{
					let voters = <Module<T>>::grandpa_authorities(); // TODO: this is wrong.
					let headers: &[T::Header] = challenge.finalized_block_proof.headers.as_slice();
					let commit = Commit {
						target_hash: challenge.finalized_block.0,
						target_number: challenge.finalized_block.1,
						precommits: challenge.finalized_block_proof.votes.clone().into_iter().map(|cv| {
							SignedPrecommit {
								precommit: Precommit::<T> {
									target_hash: *cv.vote.target().0,
									target_number: cv.vote.target().1,
								},
								signature: cv.signature,
								id: cv.authority,
							}
						}).collect(),
					};
					let ancestry_chain = AncestryChain::<T::Block>::new(headers);
					let voter_set = VoterSet::<AuthorityId>::from_iter(voters);

					if let Ok(validation_result) = validate_commit(
						&commit,
						&voter_set,
						&ancestry_chain,
					) {
						if let Some(ghost) = validation_result.ghost() {
							// TODO: I think this should check that ghost is ancestor of B.
							if *ghost != challenge.finalized_block {
								return Err("Invalid proof of finalized block")
							}
						}
					}
				}

				// Check that rejecting set doesn't have supermajority for B.
				// TODO: check signatures.
				{
					let voters = <Module<T>>::grandpa_authorities(); // TODO: this is wrong.
					let headers: &[T::Header] = challenge.rejecting_set.headers.as_slice();
					let votes = challenge.rejecting_set.votes.clone();
					let commit = Commit {
						target_hash: challenge.finalized_block.0,
						target_number: challenge.finalized_block.1,
						precommits: votes.into_iter().map(|challenged_vote| {
							SignedPrecommit {
								precommit: Precommit::<T> {
									target_hash: *challenged_vote.vote.target().0,
									target_number: challenged_vote.vote.target().1,
								},
								// TODO: This signature is OK because is not going 
								// to be checked. Maybe I can even pass None.
								signature: challenged_vote.signature,
								id: challenged_vote.authority,
							}
						}).collect(),
					};
					let ancestry_chain = AncestryChain::<T::Block>::new(headers);
					let voter_set = VoterSet::<AuthorityId>::from_iter(voters);

					if let Ok(validation_result) = validate_commit(&commit, &voter_set, &ancestry_chain) {
						if let Some(ghost) = validation_result.ghost() {
							// TODO: I think this should check that ghost is ancestor of B.
							if *ghost != challenge.finalized_block {
								return Err("Invalid proof of finalized block")
							}
						}
					}
				}

				// TODO: Punish bad guys.
			} 
			
			if round_s > round_b {
				// TODO: make same checks as above.

				// Mark previous challenge as answered.
				if let Some(challenge_hash) = challenge.previous_challenge {
					if let Some(challenge_session) = <ChallengeSessions<T>>::get(challenge_hash) {
						if challenge_session.rejecting_set_round >= round_s {
							return Err("Answer to challenge must have lower round")
						}
					}

					<ChallengeSessions<T>>::remove(challenge_hash);
				}
				
				// Push a new pending challenge session.
				let parent_hash = <system::Module<T>>::parent_hash();
				let current_height = <system::ChainContext::<T>>::default().current_height();

				let challenge_session = StoredPendingChallenge::<T> { challenge };

				let mut pending_challenges = Self::pending_challenges();
				pending_challenges.push(challenge_session);

				<PendingChallenges<T>>::put(pending_challenges);
			}
		}

		fn on_finalize(block_number: T::BlockNumber) {
			// check for scheduled pending authority set changes
			if let Some(pending_change) = <PendingChange<T>>::get() {
				// emit signal if we're at the block that scheduled the change
				if block_number == pending_change.scheduled_at {
					if let Some(median) = pending_change.forced {
						Self::deposit_log(ConsensusLog::ForcedChange(
							median,
							ScheduledChange {
								delay: pending_change.delay,
								next_authorities: pending_change.next_authorities.clone(),
							}
						))
					} else {
						Self::deposit_log(ConsensusLog::ScheduledChange(
							ScheduledChange{
								delay: pending_change.delay,
								next_authorities: pending_change.next_authorities.clone(),
							}
						));
					}
				}

				// enact the change if we've reached the enacting block
				if block_number == pending_change.scheduled_at + pending_change.delay {
					Authorities::put(&pending_change.next_authorities);
					Self::deposit_event(
						Event::NewAuthorities(pending_change.next_authorities)
					);
					<PendingChange<T>>::kill();
				}
			}

			// Clean expired challenges (and maybe slash).
			for (block_hash, challenge_session) in <ChallengeSessions<T>>::enumerate() {
				if block_number == challenge_session.scheduled_at + challenge_session.delay {

					// TODO: Slash

					<ChallengeSessions<T>>::remove(block_hash);
				}
			}

			// Process pending challenges.
			let pending_challenges = Self::pending_challenges();

			if pending_challenges.len() > 0 {

				let challenges = pending_challenges.iter()
					.map(|p| p.challenge.clone()).collect();

				// Push a digest item with the challenges.
				Self::deposit_log(ConsensusLog::Challenges(challenges));

				// Store challenge sessions.
				for pending_challenge in pending_challenges {

					let parent_hash = <system::Module<T>>::parent_hash();
					let scheduled_at = <system::ChainContext::<T>>::default().current_height();
					let delay = CHALLENGE_SESSION_LENGTH.into();

					// (TODO: Is the block hash enough for an id?)
					let challenge_hash = pending_challenge.challenge.finalized_block.0;
					let reference_block = pending_challenge.challenge.finalized_block; 
					let rejecting_set_round = pending_challenge.challenge.rejecting_set.round;

					let challenge_session = StoredChallengeSession::<T> {
						rejecting_set_round,
						challenge_hash,
						reference_block,
						parent_hash,
						scheduled_at,
						delay,
					};

					<ChallengeSessions<T>>::insert(challenge_hash, challenge_session);
				}
				<PendingChallenges<T>>::kill();
			}
			// check for scheduled pending state changes
			match <State<T>>::get() {
				StoredState::PendingPause { scheduled_at, delay } => {
					// signal change to pause
					if block_number == scheduled_at {
						Self::deposit_log(ConsensusLog::Pause(delay));
					}

					// enact change to paused state
					if block_number == scheduled_at + delay {
						<State<T>>::put(StoredState::Paused);
						Self::deposit_event(Event::Paused);
					}
				},
				StoredState::PendingResume { scheduled_at, delay } => {
					// signal change to resume
					if block_number == scheduled_at {
						Self::deposit_log(ConsensusLog::Resume(delay));
					}

					// enact change to live state
					if block_number == scheduled_at + delay {
						<State<T>>::put(StoredState::Live);
						Self::deposit_event(Event::Resumed);
					}
				},
				_ => {},
			}
		}
	}
}

impl<T: Trait> Module<T> {
	/// Get the current set of authorities, along with their respective weights.
	pub fn grandpa_authorities() -> Vec<(AuthorityId, u64)> {
		Authorities::get()
	}

	pub fn schedule_pause(in_blocks: T::BlockNumber) -> Result {
		if let StoredState::Live = <State<T>>::get() {
			let scheduled_at = system::ChainContext::<T>::default().current_height();
			<State<T>>::put(StoredState::PendingPause {
				delay: in_blocks,
				scheduled_at,
			});

			Ok(())
		} else {
			Err("Attempt to signal GRANDPA pause when the authority set isn't live \
				(either paused or already pending pause).")
		}
	}

	pub fn schedule_resume(in_blocks: T::BlockNumber) -> Result {
		if let StoredState::Paused = <State<T>>::get() {
			let scheduled_at = system::ChainContext::<T>::default().current_height();
			<State<T>>::put(StoredState::PendingResume {
				delay: in_blocks,
				scheduled_at,
			});

			Ok(())
		} else {
			Err("Attempt to signal GRANDPA resume when the authority set isn't paused \
				(either live or already pending resume).")
		}
	}

	/// Schedule a change in the authorities.
	///
	/// The change will be applied at the end of execution of the block
	/// `in_blocks` after the current block. This value may be 0, in which
	/// case the change is applied at the end of the current block.
	///
	/// If the `forced` parameter is defined, this indicates that the current
	/// set has been synchronously determined to be offline and that after
	/// `in_blocks` the given change should be applied. The given block number
	/// indicates the median last finalized block number and it should be used
	/// as the canon block when starting the new grandpa voter.
	///
	/// No change should be signaled while any change is pending. Returns
	/// an error if a change is already pending.
	pub fn schedule_change(
		next_authorities: Vec<(AuthorityId, u64)>,
		in_blocks: T::BlockNumber,
		forced: Option<T::BlockNumber>,
	) -> Result {
		if !<PendingChange<T>>::exists() {
			let scheduled_at = system::ChainContext::<T>::default().current_height();

			if let Some(_) = forced {
				if Self::next_forced().map_or(false, |next| next > scheduled_at) {
					return Err("Cannot signal forced change so soon after last.");
				}

				// only allow the next forced change when twice the window has passed since
				// this one.
				<NextForced<T>>::put(scheduled_at + in_blocks * 2.into());
			}

			<PendingChange<T>>::put(StoredPendingChange {
				delay: in_blocks,
				scheduled_at,
				next_authorities,
				forced,
			});

			Ok(())
		} else {
			Err("Attempt to signal GRANDPA change with one already pending.")
		}
	}

	/// Deposit one of this module's logs.
	fn deposit_log(log: ConsensusLog<T::Hash, T::BlockNumber, T::Header, T::Proof>) {
		let log: DigestItem<T::Hash> = DigestItem::Consensus(GRANDPA_ENGINE_ID, log.encode());
		<system::Module<T>>::deposit_log(log.into());
	}
}

impl<T: Trait> Module<T> {
	pub fn grandpa_log(digest: &DigestOf<T>) -> Option<ConsensusLog<T::Hash, T::BlockNumber, T::Header, T::Proof>> {
		let id = OpaqueDigestItemId::Consensus(&GRANDPA_ENGINE_ID);
		digest.convert_first(|l| l.try_to::<ConsensusLog<T::Hash, T::BlockNumber, T::Header, T::Proof>>(id))
	}

	pub fn pending_change(digest: &DigestOf<T>) -> Option<ScheduledChange<T::BlockNumber>>
	{
		Self::grandpa_log(digest).and_then(|signal| signal.try_into_change())
	}

	pub fn forced_change(digest: &DigestOf<T>)
		-> Option<(T::BlockNumber, ScheduledChange<T::BlockNumber>)>
	{
		Self::grandpa_log(digest).and_then(|signal| signal.try_into_forced_change())
	}

	pub fn grandpa_challenges(digest: &DigestOf<T>) -> Option<Vec<Challenge<T>>>
	{
		Self::grandpa_log(digest).and_then(|signal| signal.try_into_challenges())
	}

	pub fn pending_pause(digest: &DigestOf<T>) -> Option<T::BlockNumber>
	{
		Self::grandpa_log(digest).and_then(|signal| signal.try_into_pause())
	}

	pub fn pending_resume(digest: &DigestOf<T>) -> Option<T::BlockNumber>
	{
		Self::grandpa_log(digest).and_then(|signal| signal.try_into_resume())
	}
}

impl<T: Trait> session::OneSessionHandler<T::AccountId> for Module<T> {
	type Key = AuthorityId;

	fn on_new_session<'a, I: 'a>(changed: bool, validators: I, _queued_validators: I)
		where I: Iterator<Item=(&'a T::AccountId, AuthorityId)>
	{
		// instant changes
		if changed {
			let next_authorities = validators.map(|(_, k)| (k, 1u64)).collect::<Vec<_>>();
			let last_authorities = <Module<T>>::grandpa_authorities();
			if next_authorities != last_authorities {
				use primitives::traits::Zero;
				if let Some((further_wait, median)) = <Stalled<T>>::take() {
					let _ = Self::schedule_change(next_authorities, further_wait, Some(median));
				} else {
					let _ = Self::schedule_change(next_authorities, Zero::zero(), None);
				}
			}
		}
	}

	fn on_disabled(i: usize) {
		Self::deposit_log(ConsensusLog::OnDisabled(i as u64))
	}
}

impl<T: Trait> finality_tracker::OnFinalizationStalled<T::BlockNumber> for Module<T> {
	fn on_stalled(further_wait: T::BlockNumber, median: T::BlockNumber) {
		// when we record old authority sets, we can use `finality_tracker::median`
		// to figure out _who_ failed. until then, we can't meaningfully guard
		// against `next == last` the way that normal session changes do.
		<Stalled<T>>::put((further_wait, median));
	}
}

