// Copyright 2021 Damir Jelić
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

mod chain_key;
mod message_key;
mod messages;
mod ratchet;
mod root_key;
mod shared_secret;

use x25519_dalek::PublicKey as Curve25591PublicKey;

use chain_key::{ChainKey, RemoteChainKey};
use message_key::MessageKey;
pub(super) use messages::{OlmMessage, PrekeyMessage};
use ratchet::{Ratchet, RatchetPublicKey, RemoteRatchet};
use root_key::RemoteRootKey;

pub(super) use shared_secret::{RemoteShared3DHSecret, Shared3DHSecret};

use self::ratchet::RemoteRatchetKey;

pub(super) struct SessionKeys {
    identity_key: Curve25591PublicKey,
    ephemeral_key: Curve25591PublicKey,
    one_time_key: Curve25591PublicKey,
}

impl SessionKeys {
    pub(super) fn new(
        identity_key: Curve25591PublicKey,
        ephemeral_key: Curve25591PublicKey,
        one_time_key: Curve25591PublicKey,
    ) -> Self {
        Self {
            identity_key,
            ephemeral_key,
            one_time_key,
        }
    }
}

enum LocalDoubleRatchet {
    Inactive(InactiveDoubleRatchet),
    Active(DoubleRatchet),
}

impl LocalDoubleRatchet {
    fn advance(
        &self,
        ratchet_key: RemoteRatchetKey,
    ) -> (InactiveDoubleRatchet, RemoteDoubleRatchet) {
        if let LocalDoubleRatchet::Active(ratchet) = self {
            ratchet.advance(ratchet_key)
        } else {
            // TODO turn this into an error
            panic!("Can't advance an inactive ratchet");
        }
    }
}

struct InactiveDoubleRatchet {
    root_key: RemoteRootKey,
    ratchet_key: RemoteRatchetKey,
}

impl InactiveDoubleRatchet {
    fn activate(&self) -> DoubleRatchet {
        let (root_key, chain_key, ratchet_key) = self.root_key.advance(&self.ratchet_key);
        let dh_ratchet = Ratchet::new_with_ratchet_key(root_key, ratchet_key);

        DoubleRatchet {
            dh_ratchet,
            hkdf_ratchet: chain_key,
        }
    }
}

struct DoubleRatchet {
    dh_ratchet: Ratchet,
    hkdf_ratchet: ChainKey,
}

impl DoubleRatchet {
    fn advance(
        &self,
        ratchet_key: RemoteRatchetKey,
    ) -> (InactiveDoubleRatchet, RemoteDoubleRatchet) {
        let (root_key, remote_ratchet, remote_chain) = self.dh_ratchet.advance(ratchet_key.clone());

        let ratchet = InactiveDoubleRatchet {
            root_key,
            ratchet_key,
        };

        let remote_ratchet = RemoteDoubleRatchet {
            dh_ratchet: remote_ratchet,
            hkdf_ratchet: remote_chain,
        };

        (ratchet, remote_ratchet)
    }

    fn ratchet_key(&self) -> RatchetPublicKey {
        RatchetPublicKey::from(self.dh_ratchet.ratchet_key())
    }

    fn encrypt(&mut self, plaintext: &[u8]) -> OlmMessage {
        let message_key = self.hkdf_ratchet.create_message_key(self.ratchet_key());

        message_key.encrypt(plaintext)
    }
}

struct RemoteDoubleRatchet {
    dh_ratchet: RemoteRatchet,
    hkdf_ratchet: RemoteChainKey,
}

impl RemoteDoubleRatchet {
    fn decrypt(&mut self, message: &OlmMessage, ciphertext: &[u8], mac: [u8; 8]) -> Vec<u8> {
        let message_key = self.hkdf_ratchet.create_message_key();
        message_key.decrypt(message, ciphertext, mac)
    }

    fn belongs_to(&self, ratchet_key: &RemoteRatchetKey) -> bool {
        self.dh_ratchet.belongs_to(ratchet_key)
    }
}

pub struct Session {
    session_keys: Option<SessionKeys>,
    sending_ratchet: LocalDoubleRatchet,
    receiving_ratchet: Option<RemoteDoubleRatchet>,
}

impl Session {
    pub(super) fn new(shared_secret: Shared3DHSecret, session_keys: SessionKeys) -> Self {
        let (root_key, chain_key) = shared_secret.expand();

        let local_ratchet = Ratchet::new(root_key);

        let local_ratchet = DoubleRatchet {
            dh_ratchet: local_ratchet,
            hkdf_ratchet: chain_key,
        };

        Self {
            session_keys: Some(session_keys),
            sending_ratchet: LocalDoubleRatchet::Active(local_ratchet),
            receiving_ratchet: None,
        }
    }

    pub(super) fn new_remote(
        shared_secret: RemoteShared3DHSecret,
        remote_ratchet_key: RemoteRatchetKey,
    ) -> Self {
        let (root_key, remote_chain_key) = shared_secret.expand();

        let local_ratchet = InactiveDoubleRatchet {
            root_key,
            ratchet_key: remote_ratchet_key.clone(),
        };

        let remote_ratchet = RemoteRatchet::new(remote_ratchet_key);
        let local_ratchet = LocalDoubleRatchet::Inactive(local_ratchet);

        let remote_ratchet = RemoteDoubleRatchet {
            dh_ratchet: remote_ratchet,
            hkdf_ratchet: remote_chain_key,
        };

        Self {
            session_keys: None,
            sending_ratchet: local_ratchet,
            receiving_ratchet: Some(remote_ratchet),
        }
    }

    pub fn encrypt(&mut self, plaintext: &str) -> Vec<u8> {
        let message = match &mut self.sending_ratchet {
            LocalDoubleRatchet::Inactive(ratchet) => {
                let mut ratchet = ratchet.activate();
                let message = ratchet.encrypt(plaintext.as_bytes());
                self.sending_ratchet = LocalDoubleRatchet::Active(ratchet);
                message
            }
            LocalDoubleRatchet::Active(ratchet) => ratchet.encrypt(plaintext.as_bytes()),
        };

        if let Some(session_keys) = &self.session_keys {
            PrekeyMessage::from_parts_untyped_prost(
                session_keys.one_time_key.as_bytes().to_vec(),
                session_keys.ephemeral_key.as_bytes().to_vec(),
                session_keys.identity_key.as_bytes().to_vec(),
                message.into_vec(),
            )
            .inner
        } else {
            message.into_vec()
        }
    }

    pub fn decrypt_prekey(&mut self, message: Vec<u8>) -> Vec<u8> {
        let message = PrekeyMessage::from(message);
        let (_, _, _, message) = message.decode().unwrap();

        self.decrypt(message)
    }

    pub fn decrypt(&mut self, message: Vec<u8>) -> Vec<u8> {
        let message = OlmMessage::from(message);
        let decoded = message.decode().unwrap();

        // TODO try to use existing message keys.

        if !self
            .receiving_ratchet
            .as_ref()
            .map_or(false, |r| r.belongs_to(&decoded.ratchet_key))
        {
            let (sending_ratchet, mut remote_ratchet) =
                self.sending_ratchet.advance(decoded.ratchet_key);

            // TODO don't update the state if the message doesn't decrypt
            let plaintext = remote_ratchet.decrypt(&message, &decoded.ciphertext, decoded.mac);

            self.sending_ratchet = LocalDoubleRatchet::Inactive(sending_ratchet);
            self.receiving_ratchet = Some(remote_ratchet);
            self.session_keys = None;

            plaintext
        } else if let Some(ref mut remote_ratchet) = self.receiving_ratchet {
            remote_ratchet.decrypt(&message, &decoded.ciphertext, decoded.mac)
        } else {
            todo!()
        }
    }
}