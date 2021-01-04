// Copyright 2018-2019 Parity Technologies (UK) Ltd
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

//! Logic for voting and handling messages within a single round.

use std::{fmt::Debug, sync::Arc};

use futures::{
	channel::mpsc::UnboundedReceiver, future::FusedFuture, stream::Fuse, FutureExt, SinkExt,
	Stream, StreamExt,
};
use log::{debug, trace};

use super::Environment;
use crate::{
	round::{Round, State as RoundState},
	validate_commit,
	weights::VoteWeight,
	BlockNumberOps, Commit, HistoricalVotes, ImportResult, Message, Precommit, Prevote,
	PrimaryPropose, SignedMessage, SignedPrecommit,
};

/// The state of a voting round.
pub(super) enum State<T> {
	Start(T, T),
	Proposed(T, T),
	Prevoted(T),
	Precommitted,
}

impl<T> std::fmt::Debug for State<T> {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
		match self {
			State::Start(..) => write!(f, "Start"),
			State::Proposed(..) => write!(f, "Proposed"),
			State::Prevoted(_) => write!(f, "Prevoted"),
			State::Precommitted => write!(f, "Precommitted"),
		}
	}
}

/// Whether we should vote in the current round (i.e. push votes to the sink.)
enum Voting {
	/// Voting is disabled for the current round.
	No,
	/// Voting is enabled for the current round (prevotes and precommits.)
	Yes,
	/// Voting is enabled for the current round and we are the primary proposer
	/// (we can also push primary propose messages).
	Primary,
}

impl Voting {
	/// Whether the voter should cast round votes (prevotes and precommits.)
	fn is_active(&self) -> bool {
		match self {
			Voting::Yes | Voting::Primary => true,
			_ => false,
		}
	}

	/// Whether the voter is the primary proposer.
	fn is_primary(&self) -> bool {
		match self {
			Voting::Primary => true,
			_ => false,
		}
	}
}

/// Logic for a voter on a specific round.
pub(super) struct VotingRound<H, N, E>
where
	H: Ord,
	E: Environment<H, N>,
{
	env: Arc<E>,
	voting: Voting,
	incoming: Fuse<E::In>,
	outgoing: E::Out,
	round: Round<E::Id, H, N, E::Signature>,
	state: Option<State<E::Timer>>,
	primary_block: Option<(H, N)>,
	best_finalized: Option<Commit<H, N, E::Signature, E::Id>>,
	last_round_state: Option<RoundState<H, N>>,
	last_round_state_updates: Option<UnboundedReceiver<RoundState<H, N>>>,
}

impl<H, N, E> VotingRound<H, N, E>
where
	H: Clone + Debug + Eq + Ord,
	N: BlockNumberOps + Debug,
	E: Environment<H, N>,
{
	fn handle_vote(
		&mut self,
		vote: SignedMessage<H, N, E::Signature, E::Id>,
	) -> Result<(), E::Error> {
		let SignedMessage {
			message,
			signature,
			id,
		} = vote;

		if !self
			.env
			.is_equal_or_descendent_of(self.round.base().0, message.target().0.clone())
		{
			trace!(target: "afg",
				"Ignoring message targeting {:?} lower than round base {:?}",
				message.target(), self.round.base(),
			);

			return Ok(());
		}

		match message {
			Message::Prevote(prevote) => {
				let import_result = self
					.round
					.import_prevote(&*self.env, prevote, id, signature)?;

				if let Some(equivocation) = import_result.equivocation {
					self.env
						.prevote_equivocation(self.round.number(), equivocation);
				}
			}
			Message::Precommit(precommit) => {
				let import_result = self
					.round
					.import_precommit(&*self.env, precommit, id, signature)?;

				if let Some(equivocation) = import_result.equivocation {
					self.env
						.precommit_equivocation(self.round.number(), equivocation);
				}
			}
			Message::PrimaryPropose(primary) => {
				let primary_id = self.round.primary_voter().0.clone();

				// note that id here refers to the party which has cast the vote
				// and not the id of the party which has received the vote message.
				if id == primary_id {
					self.primary_block = Some((primary.target_hash, primary.target_number));
				}
			}
		}

		Ok(())
	}

	async fn primary_propose(
		&mut self,
		last_round_state: &RoundState<H, N>,
	) -> Result<bool, E::Error> {
		let maybe_estimate = last_round_state.estimate.clone();

		match (maybe_estimate, self.voting.is_primary()) {
			(Some(last_round_estimate), true) => {
				let maybe_finalized = last_round_state.finalized.clone();

				// Last round estimate has not been finalized.
				let should_send_primary =
					maybe_finalized.map_or(true, |f| last_round_estimate.1 > f.1);

				if should_send_primary {
					debug!(target: "afg", "Sending primary block hint for round {}", self.round.number());

					let primary = PrimaryPropose {
						target_hash: last_round_estimate.0,
						target_number: last_round_estimate.1,
					};

					self.env.proposed(self.round.number(), primary.clone())?;
					self.outgoing.send(Message::PrimaryPropose(primary)).await?;

					return Ok(true);
				} else {
					debug!(target: "afg",
						"Last round estimate has been finalized, not sending primary block hint for round {}",
						self.round.number(),
					);
				}
			}
			(None, true) => {
				debug!(target: "afg",
					"Last round estimate does not exist, not sending primary block hint for round {}",
					self.round.number(),
				);
			}
			_ => {}
		}

		Ok(false)
	}

	fn prevote(
		&mut self,
		prevote_timer_ready: bool,
		last_round_state: &RoundState<H, N>,
	) -> Result<bool, E::Error> {
		Ok(true)
	}

	fn precommit(
		&mut self,
		precommit_timer_ready: bool,
		last_round_state: &RoundState<H, N>,
	) -> Result<bool, E::Error> {
		Ok(true)
	}

	pub async fn run(mut self) -> Result<(), E::Error> {
		let mut last_round_state_updates = match self.last_round_state_updates.take() {
			Some(stream) => stream.boxed_local().fuse(),
			None => futures::stream::pending().boxed_local().fuse(),
		};

		let mut last_round_state = std::mem::take(&mut self.last_round_state);

		macro_rules! handle_inputs {
			($timer:expr) => {{
				futures::select! {
					vote = self.incoming.select_next_some() => {
						self.handle_vote(vote?)?;
					}
					state = last_round_state_updates.select_next_some() => {
						last_round_state = Some(state);
					}
					_ = &mut $timer => {}
				}
				$timer.is_terminated()
			}};
			() => {
				handle_inputs!(futures::future::pending::<()>())
			};
		}

		loop {
			match self.state.take() {
				Some(State::Start(mut prevote_timer, precommit_timer)) => {
					let prevote_timer_ready = handle_inputs!(prevote_timer);

					if let Some(last_round_state) = last_round_state.as_ref() {
						let proposed = self.primary_propose(last_round_state).await?;
						let prevoted = self.prevote(prevote_timer_ready, last_round_state)?;

						if prevoted {
							self.state = Some(State::Prevoted(precommit_timer));
						} else if proposed {
							self.state = Some(State::Proposed(prevote_timer, precommit_timer));
						} else {
							self.state = Some(State::Start(prevote_timer, precommit_timer));
						}
					} else {
						self.state = Some(State::Start(prevote_timer, precommit_timer));
					}
				}
				Some(State::Proposed(mut prevote_timer, precommit_timer)) => {
					let prevote_timer_ready = handle_inputs!(prevote_timer);

					if let Some(last_round_state) = last_round_state.as_ref() {
						let prevoted = self.prevote(prevote_timer_ready, last_round_state)?;

						if prevoted {
							self.state = Some(State::Prevoted(precommit_timer));
						} else {
							self.state = Some(State::Proposed(prevote_timer, precommit_timer));
						}
					} else {
						self.state = Some(State::Proposed(prevote_timer, precommit_timer));
					}
				}
				Some(State::Prevoted(mut precommit_timer)) => {
					let precommit_timer_ready = handle_inputs!(precommit_timer);

					if let Some(last_round_state) = last_round_state.as_ref() {
						let precommitted =
							self.precommit(precommit_timer_ready, last_round_state)?;

						if precommitted {
							self.state = Some(State::Precommitted);
						} else {
							self.state = Some(State::Prevoted(precommit_timer));
						}
					} else {
						self.state = Some(State::Prevoted(precommit_timer));
					}
				}
				Some(State::Precommitted) => {
					handle_inputs!();
					self.state = Some(State::Precommitted);
				}
				None => {
					handle_inputs!();
					self.state = None;
				}
			}

			if self.round.completable() {
				let last_round_estimate_finalized = match last_round_state {
					Some(RoundState {
						estimate: Some((_, last_round_estimate)),
						finalized: Some((_, last_round_finalized)),
						..
					}) => {
						// either it was already finalized in the previous round
						let finalized_in_last_round = last_round_estimate <= last_round_finalized;

						// or it must be finalized in the current round
						let finalized_in_current_round =
							self.round
								.finalized()
								.map_or(false, |(_, current_round_finalized)| {
									last_round_estimate <= *current_round_finalized
								});

						finalized_in_last_round || finalized_in_current_round
					}
					None => {
						// NOTE: when we catch up to a round we complete the round
						// without any last round state. in this case we already started
						// a new round after we caught up so this guard is unneeded.
						true
					}
					_ => false,
				};

				if last_round_estimate_finalized {
					// TODO: return background round
					return Ok(());
				}
			}
		}
	}
}
