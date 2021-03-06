// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Handles all over the wire message encryption and decryption upon handshake completion.

use ln::peers::{chacha, hkdf5869rfc};
use util::byte_utils;
use std::collections::VecDeque;

pub type SymmetricKey = [u8; 32];

/// Maximum Lightning message data length according to
/// [BOLT-8](https://github.com/lightningnetwork/lightning-rfc/blob/v1.0/08-transport.md#lightning-message-specification)
/// and [BOLT-1](https://github.com/lightningnetwork/lightning-rfc/blob/master/01-messaging.md#lightning-message-format):
pub const LN_MAX_MSG_LEN: usize = 65535;
pub const LN_MAX_PACKET_LENGTH: usize = MESSAGE_LENGTH_HEADER_SIZE + chacha::TAG_SIZE + LN_MAX_MSG_LEN + chacha::TAG_SIZE;

pub const MESSAGE_LENGTH_HEADER_SIZE: usize = 2;
pub const TAGGED_MESSAGE_LENGTH_HEADER_SIZE: usize = MESSAGE_LENGTH_HEADER_SIZE + chacha::TAG_SIZE;

pub const KEY_ROTATION_INDEX: u32 = 1000;

/// Instantiate a new (Encryptor, Decryptor) with specified sending and receiving keys
pub fn create_encryptor_decryptor(sending_key: SymmetricKey, receiving_key: SymmetricKey, chaining_key: SymmetricKey) -> (Encryptor, Decryptor) {
	(Encryptor {
		sending_key,
		sending_chaining_key: chaining_key,
		sending_nonce: 0
	},
	Decryptor {
		receiving_key,
		receiving_chaining_key: chaining_key,
		receiving_nonce: 0,
		read_buffer: Some(vec![]),
		pending_message_length: None,
		decrypted_payloads: VecDeque::new(),
	})
}

// Shared helper for the Encryptor and Decryptor
fn increment_nonce_helper(nonce: &mut u32, chaining_key: &mut SymmetricKey, key: &mut SymmetricKey) {
	*nonce += 1;
	if *nonce == KEY_ROTATION_INDEX {
		rotate_key(chaining_key, key);
		*nonce = 0;
	}
}

// Shared helper for the Encryptor and Decryptor
fn rotate_key(chaining_key: &mut SymmetricKey, key: &mut SymmetricKey) {
	let (new_chaining_key, new_key) = hkdf5869rfc::derive(chaining_key, key);
	chaining_key.copy_from_slice(&new_chaining_key);
	key.copy_from_slice(&new_key);
}

pub struct Encryptor {
	sending_key: SymmetricKey,
	sending_chaining_key: SymmetricKey,
	sending_nonce: u32,
}

pub struct Decryptor {
	receiving_key: SymmetricKey,
	receiving_chaining_key: SymmetricKey,
	receiving_nonce: u32,

	pending_message_length: Option<usize>,
	read_buffer: Option<Vec<u8>>,
	decrypted_payloads: VecDeque<Vec<u8>>,
}

impl Iterator for Decryptor {
	type Item = Vec<u8>;

	fn next(&mut self) -> Option<Self::Item> {
		self.decrypted_payloads.pop_front()
	}
}

impl Encryptor {
	pub fn encrypt_buf(&mut self, buffer: &[u8]) -> Vec<u8> {
		if buffer.len() > LN_MAX_MSG_LEN {
			panic!("Attempted to encrypt message longer than {} bytes!", LN_MAX_MSG_LEN);
		}

		let length = buffer.len() as u16;
		let length_bytes = byte_utils::be16_to_array(length);

		let mut ciphertext = vec![0u8; TAGGED_MESSAGE_LENGTH_HEADER_SIZE + length as usize + chacha::TAG_SIZE];

		chacha::encrypt(&self.sending_key, self.sending_nonce as u64, &[0; 0], &length_bytes, &mut ciphertext[..TAGGED_MESSAGE_LENGTH_HEADER_SIZE]);
		self.increment_nonce();

		&chacha::encrypt(&self.sending_key, self.sending_nonce as u64, &[0; 0], buffer, &mut ciphertext[TAGGED_MESSAGE_LENGTH_HEADER_SIZE..]);
		self.increment_nonce();

		ciphertext
	}

	fn increment_nonce(&mut self) {
		increment_nonce_helper(&mut self.sending_nonce, &mut self.sending_chaining_key, &mut self.sending_key);
	}
}

impl Decryptor {

	// Read in new encrypted data and process it. This attempts to decrypt the input data and any
	// existing data in the internal read buffer and can return an error if there is an error raised
	// from the decryption code.
	pub fn read(&mut self, data: &[u8]) -> Result<(), String> {
		let mut read_buffer = self.read_buffer.take().unwrap();

		let buffer = if read_buffer.is_empty() {
			data
		} else {
			read_buffer.extend_from_slice(data);
			read_buffer.as_slice()
		};

		let mut read_offset = 0;
		loop {
			match self.decrypt_next(&buffer[read_offset..]) {
				Ok((Some(result), bytes_read)) => {
					read_offset += bytes_read;
					self.decrypted_payloads.push_back(result);
				},
				Ok((None, 0)) => {
					self.read_buffer = Some(buffer[read_offset..].to_vec());
					break;
				}
				Err(e) => {
					return Err(e);
				}
				Ok((None, _)) => { panic!("Invalid return from decrypt_next()") }
			}
		}

		// If we ever get to the end of the decryption phase and have more data in the read buffer
		// than is possible for a valid message something has gone wrong. An error with a mismatched
		// length and payload should result an error from the decryption code before we get here.
		if self.read_buffer.as_ref().unwrap().len() > LN_MAX_PACKET_LENGTH {
			panic!("Encrypted message data longer than {}", LN_MAX_PACKET_LENGTH);
		}

		Ok(())
	}

	/// Decrypt the next payload from the slice returning the number of bytes consumed during the
	/// operation. This will always be (None, 0) if no payload could be decrypted.
	pub fn decrypt_next(&mut self, buffer: &[u8]) -> Result<(Option<Vec<u8>>, usize), String> {
		let message_length = if let Some(length) = self.pending_message_length {
			// we have already decrypted the header
			length
		} else {
			if buffer.len() < TAGGED_MESSAGE_LENGTH_HEADER_SIZE {
				// A message must be at least 18 bytes (2 for encrypted length, 16 for the tag)
				return Ok((None, 0));
			}

			let encrypted_length = &buffer[0..TAGGED_MESSAGE_LENGTH_HEADER_SIZE];
			let mut length_bytes = [0u8; MESSAGE_LENGTH_HEADER_SIZE];
			chacha::decrypt(&self.receiving_key, self.receiving_nonce as u64, &[0; 0], encrypted_length, &mut length_bytes)?;

			self.increment_nonce();

			// the message length
			byte_utils::slice_to_be16(&length_bytes) as usize
		};

		let message_end_index = TAGGED_MESSAGE_LENGTH_HEADER_SIZE + message_length + chacha::TAG_SIZE;

		if buffer.len() < message_end_index {
			self.pending_message_length = Some(message_length);
			return Ok((None, 0));
		}

		self.pending_message_length = None;

		let encrypted_message = &buffer[TAGGED_MESSAGE_LENGTH_HEADER_SIZE..message_end_index];
		let mut message = vec![0u8; message_length];

		chacha::decrypt(&self.receiving_key, self.receiving_nonce as u64, &[0; 0], encrypted_message, &mut message)?;

		self.increment_nonce();

		Ok((Some(message), message_end_index))
	}

	fn increment_nonce(&mut self) {
		increment_nonce_helper(&mut self.receiving_nonce, &mut self.receiving_chaining_key, &mut self.receiving_key);
	}

	// Used in tests to determine whether or not excess bytes entered the Decryptor without needing
	// to bring up infrastructure to properly encode it
	#[cfg(test)]
	pub fn read_buffer_length(&self) -> usize {
		match &self.read_buffer {
			&Some(ref vec) => { vec.len() }
			&None => 0
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use hex;

	fn setup_peers() -> ((Encryptor, Decryptor), (Encryptor, Decryptor)) {
		let chaining_key_vec = hex::decode("919219dbb2920afa8db80f9a51787a840bcf111ed8d588caf9ab4be716e42b01").unwrap();
		let mut chaining_key = [0u8; 32];
		chaining_key.copy_from_slice(&chaining_key_vec);

		let sending_key_vec = hex::decode("969ab31b4d288cedf6218839b27a3e2140827047f2c0f01bf5c04435d43511a9").unwrap();
		let mut sending_key = [0u8; 32];
		sending_key.copy_from_slice(&sending_key_vec);

		let receiving_key_vec = hex::decode("bb9020b8965f4df047e07f955f3c4b88418984aadc5cdb35096b9ea8fa5c3442").unwrap();
		let mut receiving_key = [0u8; 32];
		receiving_key.copy_from_slice(&receiving_key_vec);

		let connected_peer = create_encryptor_decryptor(sending_key, receiving_key, chaining_key);
		let remote_peer = create_encryptor_decryptor(receiving_key, sending_key, chaining_key);

		(connected_peer, remote_peer)
	}

	#[test]
	fn test_empty_message() {
		let ((mut connected_encryptor, _), (_, mut remote_decryptor)) = setup_peers();

		let message: Vec<u8> = vec![];
		let encrypted_message = connected_encryptor.encrypt_buf(&message);
		assert_eq!(encrypted_message.len(), 2 + 16 + 16);

		remote_decryptor.read(&encrypted_message[..]).unwrap();
		let decrypted_message = remote_decryptor.next().unwrap();
		assert_eq!(decrypted_message, Vec::<u8>::new());
	}

	// Test that descrypting from a slice that is the partial data followed by another decrypt call
	// with the remaining data works. This exercises the slow-path for decryption and ensures the
	// data is written to the read_buffer properly.
	#[test]
	fn test_decrypt_from_slice_two_calls_no_header_then_rest() {
		let ((mut connected_encryptor, _), (_, mut remote_decryptor)) = setup_peers();

		let message: Vec<u8> = vec![1];
		let encrypted_message = connected_encryptor.encrypt_buf(&message);

		remote_decryptor.read(&encrypted_message[..1]).unwrap();
		assert!(remote_decryptor.next().is_none());

		remote_decryptor.read(&encrypted_message[1..]).unwrap();
		let decrypted_message = remote_decryptor.next().unwrap();

		assert_eq!(decrypted_message, vec![1]);
	}

	// Include the header in the first slice
	#[test]
	fn test_decrypt_from_slice_two_calls_header_then_rest() {
		let ((mut connected_encryptor, _), (_, mut remote_decryptor)) = setup_peers();

		let message: Vec<u8> = vec![1];
		let encrypted_message = connected_encryptor.encrypt_buf(&message);

		remote_decryptor.read(&encrypted_message[..20]).unwrap();
		assert!(remote_decryptor.next().is_none());

		remote_decryptor.read(&encrypted_message[20..]).unwrap();
		let decrypted_message = remote_decryptor.next().unwrap();

		assert_eq!(decrypted_message, vec![1]);
	}

	#[test]
	fn test_nonce_chaining() {
		let ((mut connected_encryptor, _), _) = setup_peers();
		let message = hex::decode("68656c6c6f").unwrap();

		let encrypted_message = connected_encryptor.encrypt_buf(&message);
		assert_eq!(encrypted_message, hex::decode("cf2b30ddf0cf3f80e7c35a6e6730b59fe802473180f396d88a8fb0db8cbcf25d2f214cf9ea1d95").unwrap());

		// the second time the same message is encrypted, the ciphertext should be different
		let encrypted_message = connected_encryptor.encrypt_buf(&message);
		assert_eq!(encrypted_message, hex::decode("72887022101f0b6753e0c7de21657d35a4cb2a1f5cde2650528bbc8f837d0f0d7ad833b1a256a1").unwrap());
	}

	#[test]
	/// Based on RFC test vectors: https://github.com/lightningnetwork/lightning-rfc/blob/master/08-transport.md#message-encryption-tests
	fn test_key_rotation() {
		let ((mut connected_encryptor, _), _) = setup_peers();

		let message = hex::decode("68656c6c6f").unwrap();
		let mut encrypted_messages: Vec<Vec<u8>> = Vec::new();

		for _ in 0..1002 {
			let encrypted_message = connected_encryptor.encrypt_buf(&message);
			encrypted_messages.push(encrypted_message);
		}

		assert_eq!(encrypted_messages[500], hex::decode("178cb9d7387190fa34db9c2d50027d21793c9bc2d40b1e14dcf30ebeeeb220f48364f7a4c68bf8").unwrap());
		assert_eq!(encrypted_messages[501], hex::decode("1b186c57d44eb6de4c057c49940d79bb838a145cb528d6e8fd26dbe50a60ca2c104b56b60e45bd").unwrap());
		assert_eq!(encrypted_messages[1000], hex::decode("4a2f3cc3b5e78ddb83dcb426d9863d9d9a723b0337c89dd0b005d89f8d3c05c52b76b29b740f09").unwrap());
		assert_eq!(encrypted_messages[1001], hex::decode("2ecd8c8a5629d0d02ab457a0fdd0f7b90a192cd46be5ecb6ca570bfc5e268338b1a16cf4ef2d36").unwrap());
	}

	#[test]
	fn test_decryption_buffering() {
		let ((mut connected_encryptor, _), (_, mut remote_decryptor)) = setup_peers();

		let message = hex::decode("68656c6c6f").unwrap();
		let mut encrypted_messages: Vec<Vec<u8>> = Vec::new();

		for _ in 0..1002 {
			let encrypted_message = connected_encryptor.encrypt_buf(&message);
			encrypted_messages.push(encrypted_message);
		}

		for _ in 0..501 {
			// read two messages at once, filling buffer
			let mut current_encrypted_message = encrypted_messages.remove(0);
			let next_encrypted_message = encrypted_messages.remove(0);
			current_encrypted_message.extend_from_slice(&next_encrypted_message);
			remote_decryptor.read(&current_encrypted_message[..]).unwrap();

			let decrypted_message = remote_decryptor.next().unwrap();
			assert_eq!(decrypted_message, message);
		}

		for _ in 0..501 {
			// decrypt messages directly from buffer without adding to it
			remote_decryptor.read(&[]).unwrap();
			let decrypted_message = remote_decryptor.next().unwrap();
			assert_eq!(decrypted_message, message);
		}
	}

	// Decryption errors should result in Err
	#[test]
	fn decryption_failure_errors() {
		let ((mut connected_encryptor, _), (_, mut remote_decryptor)) = setup_peers();
		let encrypted = connected_encryptor.encrypt_buf(&[1]);

		remote_decryptor.receiving_key = [0; 32];
		assert_eq!(remote_decryptor.read(&encrypted), Err("invalid hmac".to_string()));
	}

	// Test next()::None
	#[test]
	fn decryptor_iterator_empty() {
		let (_, (_, mut remote_decryptor)) = setup_peers();

		assert_eq!(remote_decryptor.next(), None);
	}

	// Test next() -> next()::None
	#[test]
	fn decryptor_iterator_one_item_valid() {
		let ((mut connected_encryptor, _), (_, mut remote_decryptor)) = setup_peers();
		let encrypted = connected_encryptor.encrypt_buf(&[1]);
		remote_decryptor.read(&encrypted).unwrap();

		assert_eq!(remote_decryptor.next(), Some(vec![1]));
		assert_eq!(remote_decryptor.next(), None);
	}

	#[test]
	// https://github.com/lightningnetwork/lightning-rfc/blob/v1.0/08-transport.md#lightning-message-specification
	fn max_msg_len_limit_value() {
		assert_eq!(LN_MAX_MSG_LEN, 65535);
		assert_eq!(LN_MAX_PACKET_LENGTH, 65569);
	}

	#[test]
	#[should_panic(expected = "Attempted to encrypt message longer than 65535 bytes!")]
	fn max_message_len_encryption() {
		let ((mut connected_encryptor, _), _) = setup_peers();
		let msg = [4u8; LN_MAX_MSG_LEN + 1];
		let _should_panic = connected_encryptor.encrypt_buf(&msg);
	}

	// Test that the decryptor can handle multiple partial reads() that result in a total size
	// larger than LN_MAX_PACKET_LENGTH and still decrypt the messages.
	#[test]
	fn read_buffer_can_grow_over_max_payload_len() {
		let ((mut connected_encryptor, _), ( _, mut remote_decryptor)) = setup_peers();
		let msg1 = [1u8; LN_MAX_MSG_LEN];
		let msg2 = [2u8; LN_MAX_MSG_LEN];

		let encrypted1 = connected_encryptor.encrypt_buf(&msg1);
		let encrypted2 = connected_encryptor.encrypt_buf(&msg2);

		let read1 = &encrypted1[..1];
		let mut read2 = vec![];
		read2.extend_from_slice(&encrypted1[1..]);
		read2.extend_from_slice(&encrypted2);

		remote_decryptor.read(read1).unwrap();
		assert_eq!(remote_decryptor.next(), None);

		remote_decryptor.read(&read2[..]).unwrap();

		assert_eq!(remote_decryptor.next(), Some(msg1.to_vec()));
		assert_eq!(remote_decryptor.next(), Some(msg2.to_vec()));
	}
}