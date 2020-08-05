// maelstrom
// Copyright (C) 2020 Raphael Robert
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see http://www.gnu.org/licenses/.

use crate::codec::*;
use crate::crypto::aead;
use crate::crypto::hash::*;
use crate::extensions::*;
use crate::messages::*;
use crate::schedule::*;
use crate::treemath::*;

const OUT_OF_ORDER_TOLERANCE: u32 = 5;
const MAXIMUM_FORWARD_DISTANCE: u32 = 1000;

#[derive(Debug, PartialEq)]
pub enum ASError {
    TooDistantInThePast,
    TooDistantInTheFuture,
    IndexOutOfBounds,
}

fn derive_app_secret(
    ciphersuite: CipherSuite,
    secret: &[u8],
    label: &str,
    node: u32,
    generation: u32,
    length: usize,
) -> Vec<u8> {
    let application_context = ApplicationContext { node, generation };
    let serialized_application_context = application_context.encode_detached().unwrap();
    hkdf_expand_label(
        ciphersuite,
        secret,
        label,
        &serialized_application_context,
        length,
    )
}

#[derive(Debug, PartialEq)]
pub struct ApplicationSecrets {
    pub nonce: aead::Nonce,
    pub key: aead::AEADKey,
}

struct ApplicationContext {
    node: u32,
    generation: u32,
}

impl Codec for ApplicationContext {
    fn encode(&self, buffer: &mut Vec<u8>) -> Result<(), CodecError> {
        self.node.encode(buffer)?;
        self.generation.encode(buffer)?;
        Ok(())
    }
    fn decode(cursor: &mut Cursor) -> Result<Self, CodecError> {
        let node = u32::decode(cursor)?;
        let generation = u32::decode(cursor)?;
        Ok(ApplicationContext { node, generation })
    }
}

#[derive(Clone)]
struct ASTreeNode {
    pub secret: Vec<u8>,
}

impl Codec for ASTreeNode {
    fn encode(&self, buffer: &mut Vec<u8>) -> Result<(), CodecError> {
        encode_vec(VecSize::VecU8, buffer, &self.secret)?;
        Ok(())
    }
    fn decode(cursor: &mut Cursor) -> Result<Self, CodecError> {
        let secret = decode_vec(VecSize::VecU8, cursor)?;
        Ok(ASTreeNode { secret })
    }
}

struct SenderRatchet {
    ciphersuite: CipherSuite,
    index: RosterIndex,
    generation: u32,
    past_secrets: Vec<Vec<u8>>,
}

impl Codec for SenderRatchet {
    fn encode(&self, buffer: &mut Vec<u8>) -> Result<(), CodecError> {
        self.ciphersuite.encode(buffer)?;
        self.index.as_u32().encode(buffer)?;
        self.generation.encode(buffer)?;
        let len = self.past_secrets.len();
        (len as u32).encode(buffer)?;
        for i in 0..len {
            encode_vec(VecSize::VecU8, buffer, &self.past_secrets[i])?;
        }
        Ok(())
    }
    fn decode(cursor: &mut Cursor) -> Result<Self, CodecError> {
        let ciphersuite = CipherSuite::decode(cursor)?;
        let index = RosterIndex::from(u32::decode(cursor)?);
        let generation = u32::decode(cursor)?;
        let len = u32::decode(cursor)? as usize;
        let mut past_secrets = vec![];
        for _ in 0..len {
            let secret = decode_vec(VecSize::VecU8, cursor)?;
            past_secrets.push(secret);
        }
        Ok(SenderRatchet {
            ciphersuite,
            index,
            generation,
            past_secrets,
        })
    }
}

impl SenderRatchet {
    pub fn new(index: RosterIndex, secret: &[u8], ciphersuite: CipherSuite) -> Self {
        Self {
            ciphersuite,
            index,
            generation: 0,
            past_secrets: vec![secret.to_vec()],
        }
    }
    pub fn get_secret(&mut self, generation: u32) -> Result<ApplicationSecrets, ASError> {
        if generation > (self.generation + MAXIMUM_FORWARD_DISTANCE) {
            return Err(ASError::TooDistantInTheFuture);
        }
        if generation < self.generation && (self.generation - generation) >= OUT_OF_ORDER_TOLERANCE
        {
            return Err(ASError::TooDistantInThePast);
        }
        if generation <= self.generation {
            let window_index =
                (self.past_secrets.len() as u32 - (self.generation - generation) - 1) as usize;
            let secret = self.past_secrets.get(window_index).unwrap().clone();
            let application_secrets = self.derive_key_nonce(&secret, generation);
            Ok(application_secrets)
        } else {
            for _ in 0..(generation - self.generation) {
                if self.past_secrets.len() == OUT_OF_ORDER_TOLERANCE as usize {
                    self.past_secrets.remove(0);
                }
                let new_secret = self.ratchet_secret(self.past_secrets.last().unwrap());
                self.past_secrets.push(new_secret);
            }
            let secret = self.past_secrets.last().unwrap();
            let application_secrets = self.derive_key_nonce(&secret, generation);
            self.generation = generation;
            Ok(application_secrets)
        }
    }
    fn ratchet_secret(&self, secret: &[u8]) -> Vec<u8> {
        let hash_len = hash_length(self.ciphersuite.into());
        derive_app_secret(
            self.ciphersuite,
            secret,
            "app-secret",
            self.index.as_u32(),
            self.generation,
            hash_len,
        )
    }
    fn derive_key_nonce(&self, secret: &[u8], generation: u32) -> ApplicationSecrets {
        let nonce = derive_app_secret(
            self.ciphersuite,
            secret,
            "app-nonce",
            self.index.as_u32(),
            generation,
            aead::Nonce::nonce_length(self.ciphersuite.into()).unwrap(),
        );
        let key = derive_app_secret(
            self.ciphersuite,
            secret,
            "app-key",
            self.index.as_u32(),
            generation,
            aead::AEADKey::key_length(self.ciphersuite.into()).unwrap(),
        );
        ApplicationSecrets {
            nonce: aead::Nonce::from_slice(&nonce).unwrap(),
            key: aead::AEADKey::from_slice(self.ciphersuite.into(), &key).unwrap(),
        }
    }
}

pub struct ASTree {
    ciphersuite: CipherSuite,
    nodes: Vec<Option<ASTreeNode>>,
    sender_ratchets: Vec<Option<SenderRatchet>>,
    size: RosterIndex,
}

impl Codec for ASTree {
    fn encode(&self, buffer: &mut Vec<u8>) -> Result<(), CodecError> {
        self.ciphersuite.encode(buffer)?;
        encode_vec(VecSize::VecU32, buffer, &self.nodes)?;
        encode_vec(VecSize::VecU32, buffer, &self.sender_ratchets)?;
        self.size.as_u32().encode(buffer)?;
        Ok(())
    }
    fn decode(cursor: &mut Cursor) -> Result<Self, CodecError> {
        let ciphersuite = CipherSuite::decode(cursor)?;
        let nodes = decode_vec(VecSize::VecU32, cursor)?;
        let sender_ratchets = decode_vec(VecSize::VecU32, cursor)?;
        let size = RosterIndex::from(u32::decode(cursor)?);
        Ok(ASTree {
            ciphersuite,
            nodes,
            sender_ratchets,
            size,
        })
    }
}

impl ASTree {
    pub fn new(ciphersuite: CipherSuite, application_secret: &[u8], size: RosterIndex) -> Self {
        let root = root(size);
        let num_indices = TreeIndex::from(size).as_usize() - 1;
        let mut nodes: Vec<Option<ASTreeNode>> = Vec::with_capacity(num_indices);
        for _ in 0..(num_indices) {
            nodes.push(None);
        }
        nodes[root.as_usize()] = Some(ASTreeNode {
            secret: application_secret.to_vec(),
        });
        let mut sender_ratchets: Vec<Option<SenderRatchet>> = Vec::with_capacity(size.as_usize());
        for _ in 0..(size.as_usize()) {
            sender_ratchets.push(None);
        }
        Self {
            ciphersuite,
            nodes,
            sender_ratchets,
            size,
        }
    }
    pub fn get_generation(&self, sender: RosterIndex) -> u32 {
        if let Some(sender_ratchet) = &self.sender_ratchets[sender.as_usize()] {
            sender_ratchet.generation
        } else {
            0
        }
    }
    pub fn get_secret(
        &mut self,
        index: RosterIndex,
        generation: u32,
    ) -> Result<ApplicationSecrets, ASError> {
        let index_in_tree = TreeIndex::from(index);
        if index >= self.size {
            return Err(ASError::IndexOutOfBounds);
        }
        if let Some(ratchet_opt) = self.sender_ratchets.get_mut(index.as_usize()) {
            if let Some(ratchet) = ratchet_opt {
                return ratchet.get_secret(generation);
            }
        }
        let mut dir_path = vec![index_in_tree];
        dir_path.extend(dirpath(index_in_tree, self.size));
        dir_path.push(root(self.size));
        let mut empty_nodes: Vec<TreeIndex> = vec![];
        for n in dir_path {
            empty_nodes.push(n);
            if self.nodes[n.as_usize()].is_some() {
                break;
            }
        }
        empty_nodes.remove(0);
        empty_nodes.reverse();
        for n in empty_nodes {
            self.hash_down(n);
        }
        let node_secret = &self.nodes[index_in_tree.as_usize()].clone().unwrap().secret;
        let mut sender_ratchet = SenderRatchet::new(index, node_secret, self.ciphersuite);
        let application_secret = sender_ratchet.get_secret(generation);
        self.nodes[index_in_tree.as_usize()] = None;
        self.sender_ratchets[index.as_usize()] = Some(sender_ratchet);
        application_secret
    }
    fn hash_down(&mut self, index_in_tree: TreeIndex) {
        let hash_len = hash_length(self.ciphersuite.into());
        let node_secret = &self.nodes[index_in_tree.as_usize()].clone().unwrap().secret;
        let left_index = left(index_in_tree);
        let right_index = right(index_in_tree, self.size);
        let left_secret = derive_app_secret(
            self.ciphersuite,
            &node_secret,
            "tree",
            left_index.as_u32(),
            0,
            hash_len,
        );
        let right_secret = derive_app_secret(
            self.ciphersuite,
            &node_secret,
            "tree",
            right_index.as_u32(),
            0,
            hash_len,
        );
        self.nodes[left_index.as_usize()] = Some(ASTreeNode {
            secret: left_secret,
        });
        self.nodes[right_index.as_usize()] = Some(ASTreeNode {
            secret: right_secret,
        });
        self.nodes[index_in_tree.as_usize()] = None;
    }
}

#[test]
fn test_boundaries() {
    let ciphersuite = CipherSuite::MLS10_128_HPKEX25519_CHACHA20POLY1305_SHA256_Ed25519;
    let mut astree = ASTree::new(ciphersuite, &[0u8; 32], RosterIndex::from(2u32));
    assert!(astree.get_secret(RosterIndex::from(0u32), 0).is_ok());
    assert!(astree.get_secret(RosterIndex::from(1u32), 0).is_ok());
    assert!(astree.get_secret(RosterIndex::from(0u32), 1).is_ok());
    assert!(astree.get_secret(RosterIndex::from(0u32), 1_000).is_ok());
    assert_eq!(
        astree.get_secret(RosterIndex::from(1u32), 1001),
        Err(ASError::TooDistantInTheFuture)
    );
    assert!(astree.get_secret(RosterIndex::from(0u32), 996).is_ok());
    assert_eq!(
        astree.get_secret(RosterIndex::from(0u32), 995),
        Err(ASError::TooDistantInThePast)
    );
    assert_eq!(
        astree.get_secret(RosterIndex::from(2u32), 0),
        Err(ASError::IndexOutOfBounds)
    );
    let mut largetree = ASTree::new(ciphersuite, &[0u8; 32], RosterIndex::from(100_000u32));
    assert!(largetree.get_secret(RosterIndex::from(0u32), 0).is_ok());
    assert!(largetree
        .get_secret(RosterIndex::from(99_999u32), 0)
        .is_ok());
    assert!(largetree
        .get_secret(RosterIndex::from(99_999u32), 1_000)
        .is_ok());
    assert_eq!(
        largetree.get_secret(RosterIndex::from(100_000u32), 0),
        Err(ASError::IndexOutOfBounds)
    );
}
