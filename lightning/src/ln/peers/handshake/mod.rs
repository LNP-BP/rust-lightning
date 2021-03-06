// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Execute handshakes for peer-to-peer connection establishment.
//! Handshake states can be advanced automatically, or by manually calling the appropriate step.
//! Once complete, returns an instance of CompletedHandshakeInfo.

use bitcoin::secp256k1::{PublicKey, SecretKey};

use ln::peers::encryption::{Decryptor, Encryptor};
use ln::peers::handshake::acts::Act;
use ln::peers::handshake::states::HandshakeState;
use ln::peers::transport::IPeerHandshake;

mod acts;
mod states;

/// Interface used by PeerHandshake to interact with NOISE state machine.
/// State may transition to the same state in the event there are not yet enough bytes to move
/// forward with the handshake.
trait IHandshakeState {
	/// Returns the next HandshakeState after processing the input bytes
	fn next(self, input: &[u8]) -> Result<(Option<Act>, HandshakeState), String>;
}

/// Object for managing handshakes.
/// Currently requires explicit ephemeral private key specification.
pub struct PeerHandshake {
	state: Option<HandshakeState>,
	ready_to_process: bool,
}

/// Container for the information returned from a successfully completed handshake
pub struct CompletedHandshakeInfo {
	pub decryptor: Decryptor,
	pub encryptor: Encryptor,
	pub their_node_id: PublicKey,
}

impl IPeerHandshake for PeerHandshake {
	/// Instantiate a handshake given the peer's static public key. The ephemeral private key MUST
	/// generate a new session with strong cryptographic randomness.
	fn new_outbound(initiator_static_private_key: &SecretKey, responder_static_public_key: &PublicKey, initiator_ephemeral_private_key: &SecretKey) -> Self {
		let state = HandshakeState::new_initiator(initiator_static_private_key, responder_static_public_key, initiator_ephemeral_private_key);

		Self {
			state: Some(state),
			ready_to_process: false,
		}
	}

	/// Initializes the outbound handshake and provides the initial bytes to send to the responder
	fn set_up_outbound(&mut self) -> Vec<u8> {
		assert!(!self.ready_to_process);
		self.ready_to_process = true;

		// This transition does not have a failure path
		let (response_vec_option, completed_handshake_info) = self.process_act(&[]).unwrap();
		assert!(completed_handshake_info.is_none());

		response_vec_option.unwrap()
	}

	/// Instantiate a new handshake in anticipation of a peer's first handshake act
	fn new_inbound(responder_static_private_key: &SecretKey, responder_ephemeral_private_key: &SecretKey) -> Self {
		Self {
			state: Some(HandshakeState::new_responder(responder_static_private_key, responder_ephemeral_private_key)),
			ready_to_process: true,
		}
	}

	/// Process act dynamically
	/// # Arguments
	/// `input`: Byte slice received from peer as part of the handshake protocol
	///
	/// # Return values
	/// Returns a tuple with the following components:
	/// `.0`: Byte vector containing the next act to send back to the peer per the handshake protocol
	/// `.1`: Some(CompleteHandshakeInfo) if the handshake was just processed to completion and messages can now be encrypted and decrypted
	fn process_act(&mut self, input: &[u8]) -> Result<(Option<Vec<u8>>, Option<CompletedHandshakeInfo>), String> {
		assert!(self.ready_to_process);
		let cur_state = self.state.take().unwrap();

		let (act_option, mut next_state) = match cur_state.next(input)? {
			(Some(act), next_state) => (Some(act.to_vec()), next_state),
			(None, next_state) => (None, next_state)
		};

		let result = match next_state {
			HandshakeState::Complete(ref mut complete_handshake_info) => {
				Ok((act_option, complete_handshake_info.take()))
			},
			_ => { Ok((act_option, None)) }
		};

		self.state = Some(next_state);

		result
	}
}

#[cfg(test)]
mod test {
	use super::*;

	use bitcoin::secp256k1;
	use bitcoin::secp256k1::key::{PublicKey, SecretKey};

	struct TestCtx {
		act1: Vec<u8>,
		outbound_handshake: PeerHandshake,
		outbound_static_public_key: PublicKey,
		inbound_handshake: PeerHandshake,
		inbound_static_public_key: PublicKey
	}

	impl TestCtx {
		fn new() -> TestCtx {
			let curve = secp256k1::Secp256k1::new();

			let outbound_static_private_key = SecretKey::from_slice(&[0x_11_u8; 32]).unwrap();
			let outbound_static_public_key = PublicKey::from_secret_key(&curve, &outbound_static_private_key);
			let outbound_ephemeral_private_key = SecretKey::from_slice(&[0x_12_u8; 32]).unwrap();

			let inbound_static_private_key = SecretKey::from_slice(&[0x_21_u8; 32]).unwrap();
			let inbound_static_public_key = PublicKey::from_secret_key(&curve, &inbound_static_private_key);
			let inbound_ephemeral_private_key = SecretKey::from_slice(&[0x_22_u8; 32]).unwrap();

			let mut outbound_handshake= PeerHandshake::new_outbound(&outbound_static_private_key, &inbound_static_public_key, &outbound_ephemeral_private_key);
			let act1 = outbound_handshake.set_up_outbound();
			let inbound_handshake = PeerHandshake::new_inbound(&inbound_static_private_key, &inbound_ephemeral_private_key);

			TestCtx {
				act1,
				outbound_handshake,
				outbound_static_public_key,
				inbound_handshake,
				inbound_static_public_key,
			}
		}
	}

	macro_rules! do_process_act_or_panic {
		($handshake:expr, $input:expr) => {
			$handshake.process_act($input).unwrap().0.unwrap()
		}
	}

	// Test that the outbound needs to call set_up_outbound() before process_act()
	#[test]
	#[should_panic(expected = "assertion failed: self.ready_to_process")]
	fn new_outbound_no_set_up_panics() {
		let curve = secp256k1::Secp256k1::new();

		let outbound_static_private_key = SecretKey::from_slice(&[0x_11_u8; 32]).unwrap();
		let outbound_ephemeral_private_key = SecretKey::from_slice(&[0x_12_u8; 32]).unwrap();
		let inbound_static_private_key = SecretKey::from_slice(&[0x_21_u8; 32]).unwrap();
		let inbound_static_public_key = PublicKey::from_secret_key(&curve, &inbound_static_private_key);

		let mut outbound_handshake= PeerHandshake::new_outbound(&outbound_static_private_key, &inbound_static_public_key, &outbound_ephemeral_private_key);
		outbound_handshake.process_act(&[]).unwrap();
	}

	// Test that calling set_up_outbound() on the inbound panics
	#[test]
	#[should_panic(expected = "assertion failed: !self.ready_to_process")]
	fn new_inbound_calling_set_up_panics() {
		let inbound_static_private_key = SecretKey::from_slice(&[0x_21_u8; 32]).unwrap();
		let inbound_ephemeral_private_key = SecretKey::from_slice(&[0x_22_u8; 32]).unwrap();

		let mut inbound_handshake = PeerHandshake::new_inbound(&inbound_static_private_key, &inbound_ephemeral_private_key);
		inbound_handshake.set_up_outbound();
	}

	// Default Outbound::Uninitiated
	#[test]
	fn new_outbound() {
		let test_ctx = TestCtx::new();

		assert_matches!(test_ctx.outbound_handshake.state, Some(HandshakeState::InitiatorAwaitingActTwo(_)));
	}

	// Default Inbound::AwaitingActOne
	#[test]
	fn new_inbound() {
		let test_ctx = TestCtx::new();

		assert_matches!(test_ctx.inbound_handshake.state, Some(HandshakeState::ResponderAwaitingActOne(_)));
	}

	/*
	 * PeerHandshake::process_act() tests
	 */

	// Full sequence from initiator and responder as a sanity test. State machine is tested in states.rs
	#[test]
	fn full_sequence_sanity_test() {
		let mut test_ctx = TestCtx::new();
		let act2 = do_process_act_or_panic!(test_ctx.inbound_handshake, &test_ctx.act1);

		let (act3, inbound_remote_pubkey) = if let (Some(act3), Some(completed_handshake_info)) = test_ctx.outbound_handshake.process_act(&act2).unwrap() {
			(act3, completed_handshake_info.their_node_id)
		} else {
			panic!();
		};

		let outbound_remote_pubkey = if let (None, Some(completed_handshake_info)) = test_ctx.inbound_handshake.process_act(&act3).unwrap() {
			completed_handshake_info.their_node_id
		} else {
			panic!();
		};

		assert_eq!(inbound_remote_pubkey, test_ctx.inbound_static_public_key);
		assert_eq!(outbound_remote_pubkey, test_ctx.outbound_static_public_key);
	}

	// Test that the internal state object matches the return from state_machine.next()
	// This could make use of a mocking library to remove the dependency on the state machine. All
	// that needs to be tested is that the expected state (returned) from state_machine.next() matchse
	// the internal set state.
	#[test]
	fn process_act_properly_updates_state() {
		let mut test_ctx = TestCtx::new();
		do_process_act_or_panic!(test_ctx.inbound_handshake, &test_ctx.act1);
		assert_matches!(test_ctx.inbound_handshake.state, Some(HandshakeState::ResponderAwaitingActThree(_)));
	}

	// Test that any errors from the state machine are passed back to the caller
	// This could make use of a mocking library to remove the dependency on the state machine
	// logic. All that needs to be tested is that an error from state_machine.next()
	// results in an error in process_act()
	#[test]
	fn errors_properly_returned() {
		let mut test_ctx = TestCtx::new();
		let invalid_act1 = [0; 50];
		assert_matches!(test_ctx.inbound_handshake.process_act(&invalid_act1).err(), Some(_));
	}

	// Test that any use of the PeerHandshake after returning an error panics
	#[test]
	#[should_panic(expected = "called `Option::unwrap()` on a `None` value")]
	fn use_after_error_panics() {
		let mut test_ctx = TestCtx::new();
		let invalid_act1 = [0; 50];
		assert_matches!(test_ctx.inbound_handshake.process_act(&invalid_act1).err(), Some(_));
		test_ctx.inbound_handshake.process_act(&[]).unwrap();
	}
}
