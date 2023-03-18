use core::marker::PhantomData;
use std::collections::HashMap;

use zeroize::Zeroizing;

use rand_core::SeedableRng;
use rand_chacha::ChaCha20Rng;

use transcript::{Transcript, RecommendedTranscript};
use group::GroupEncoding;
use frost::{
  curve::Ciphersuite,
  dkg::{Participant, ThresholdParams, ThresholdCore, ThresholdKeys, encryption::*, frost::*},
};

use log::info;

use serai_client::validator_sets::primitives::ValidatorSetInstance;
use messages::key_gen::*;

use crate::{DbTxn, Db, coins::Coin};

#[derive(Debug)]
pub enum KeyGenEvent<C: Ciphersuite> {
  KeyConfirmed { activation_number: usize, keys: ThresholdKeys<C> },
  ProcessorMessage(ProcessorMessage),
}

#[derive(Clone, Debug)]
struct KeyGenDb<C: Coin, D: Db>(D, PhantomData<C>);
impl<C: Coin, D: Db> KeyGenDb<C, D> {
  fn key_gen_key(dst: &'static [u8], key: impl AsRef<[u8]>) -> Vec<u8> {
    D::key(b"KEY_GEN", dst, key)
  }

  fn params_key(set: &ValidatorSetInstance) -> Vec<u8> {
    Self::key_gen_key(b"params", bincode::serialize(set).unwrap())
  }
  fn save_params(
    &mut self,
    txn: &mut D::Transaction,
    set: &ValidatorSetInstance,
    params: &ThresholdParams,
  ) {
    txn.put(Self::params_key(set), bincode::serialize(params).unwrap());
  }
  fn params(&self, set: &ValidatorSetInstance) -> ThresholdParams {
    // Directly unwraps the .get() as this will only be called after being set
    bincode::deserialize(&self.0.get(Self::params_key(set)).unwrap()).unwrap()
  }

  // Not scoped to the set since that'd have latter attempts overwrite former
  // A former attempt may become the finalized attempt, even if it doesn't in a timely manner
  // Overwriting its commitments would be accordingly poor
  fn commitments_key(id: &KeyGenId) -> Vec<u8> {
    Self::key_gen_key(b"commitments", bincode::serialize(id).unwrap())
  }
  fn save_commitments(
    &mut self,
    txn: &mut D::Transaction,
    id: &KeyGenId,
    commitments: &HashMap<Participant, Vec<u8>>,
  ) {
    txn.put(Self::commitments_key(id), bincode::serialize(commitments).unwrap());
  }
  fn commitments(
    &self,
    id: &KeyGenId,
    params: ThresholdParams,
  ) -> HashMap<Participant, EncryptionKeyMessage<C::Curve, Commitments<C::Curve>>> {
    bincode::deserialize::<HashMap<Participant, Vec<u8>>>(
      &self.0.get(Self::commitments_key(id)).unwrap(),
    )
    .unwrap()
    .drain()
    .map(|(i, bytes)| {
      (
        i,
        EncryptionKeyMessage::<C::Curve, Commitments<C::Curve>>::read::<&[u8]>(
          &mut bytes.as_ref(),
          params,
        )
        .unwrap(),
      )
    })
    .collect()
  }

  fn generated_keys_key(id: &KeyGenId) -> Vec<u8> {
    Self::key_gen_key(b"generated_keys", bincode::serialize(id).unwrap())
  }
  fn save_keys(&mut self, txn: &mut D::Transaction, id: &KeyGenId, keys: &ThresholdCore<C::Curve>) {
    txn.put(Self::generated_keys_key(id), keys.serialize());
  }

  fn keys_key(key: &<C::Curve as Ciphersuite>::G) -> Vec<u8> {
    Self::key_gen_key(b"keys", key.to_bytes())
  }
  fn confirm_keys(&mut self, txn: &mut D::Transaction, id: &KeyGenId) -> ThresholdKeys<C::Curve> {
    let keys_vec = self.0.get(Self::generated_keys_key(id)).unwrap();
    let mut keys =
      ThresholdKeys::new(ThresholdCore::read::<&[u8]>(&mut keys_vec.as_ref()).unwrap());
    C::tweak_keys(&mut keys);
    txn.put(Self::keys_key(&keys.group_key()), keys_vec);
    keys
  }
  fn keys(&self, key: &<C::Curve as Ciphersuite>::G) -> ThresholdKeys<C::Curve> {
    let mut keys = ThresholdKeys::new(
      ThresholdCore::read::<&[u8]>(&mut self.0.get(Self::keys_key(key)).unwrap().as_ref()).unwrap(),
    );
    C::tweak_keys(&mut keys);
    keys
  }
}

/// Coded so if the processor spontaneously reboots, one of two paths occur:
/// 1) It either didn't send its response, so the attempt will be aborted
/// 2) It did send its response, and has locally saved enough data to continue
#[derive(Debug)]
pub struct KeyGen<C: Coin, D: Db> {
  db: KeyGenDb<C, D>,
  entropy: Zeroizing<[u8; 32]>,

  active_commit: HashMap<ValidatorSetInstance, SecretShareMachine<C::Curve>>,
  active_share: HashMap<ValidatorSetInstance, KeyMachine<C::Curve>>,
}

impl<C: Coin, D: Db> KeyGen<C, D> {
  #[allow(clippy::new_ret_no_self)]
  pub fn new(db: D, entropy: Zeroizing<[u8; 32]>) -> KeyGen<C, D> {
    KeyGen {
      db: KeyGenDb(db, PhantomData::<C>),
      entropy,

      active_commit: HashMap::new(),
      active_share: HashMap::new(),
    }
  }

  pub fn keys(&self, key: &<C::Curve as Ciphersuite>::G) -> ThresholdKeys<C::Curve> {
    self.db.keys(key)
  }

  pub async fn handle(&mut self, msg: CoordinatorMessage) -> KeyGenEvent<C::Curve> {
    let context = |id: &KeyGenId| {
      // TODO2: Also embed the chain ID/genesis block
      format!(
        "Serai Key Gen. Session: {}, Index: {}, Attempt: {}",
        id.set.session.0, id.set.index.0, id.attempt
      )
    };

    let rng = |label, id: KeyGenId| {
      let mut transcript = RecommendedTranscript::new(label);
      transcript.append_message(b"entropy", self.entropy.as_ref());
      transcript.append_message(b"context", context(&id));
      ChaCha20Rng::from_seed(transcript.rng_seed(b"rng"))
    };
    let coefficients_rng = |id| rng(b"Key Gen Coefficients", id);
    let secret_shares_rng = |id| rng(b"Key Gen Secret Shares", id);
    let share_rng = |id| rng(b"Key Gen Share", id);

    let key_gen_machine = |id, params| {
      KeyGenMachine::new(params, context(&id)).generate_coefficients(&mut coefficients_rng(id))
    };

    match msg {
      CoordinatorMessage::GenerateKey { id, params } => {
        info!("Generating new key. ID: {:?} Params: {:?}", id, params);

        // Remove old attempts
        if self.active_commit.remove(&id.set).is_none() &&
          self.active_share.remove(&id.set).is_none()
        {
          // If we haven't handled this set before, save the params
          // This may overwrite previously written params if we rebooted, yet that isn't a
          // concern
          let mut txn = self.db.0.txn();
          self.db.save_params(&mut txn, &id.set, &params);
          txn.commit();
        }

        let (machine, commitments) = key_gen_machine(id, params);
        self.active_commit.insert(id.set, machine);

        KeyGenEvent::ProcessorMessage(ProcessorMessage::Commitments {
          id,
          commitments: commitments.serialize(),
        })
      }

      CoordinatorMessage::Commitments { id, commitments } => {
        info!("Received commitments for {:?}", id);

        if self.active_share.contains_key(&id.set) {
          // We should've been told of a new attempt before receiving commitments again
          // The coordinator is either missing messages or repeating itself
          // Either way, it's faulty
          panic!("commitments when already handled commitments");
        }

        let params = self.db.params(&id.set);

        // Parse the commitments
        let parsed = match commitments
          .iter()
          .map(|(i, commitments)| {
            EncryptionKeyMessage::<C::Curve, Commitments<C::Curve>>::read::<&[u8]>(
              &mut commitments.as_ref(),
              params,
            )
            .map(|commitments| (*i, commitments))
          })
          .collect()
        {
          Ok(commitments) => commitments,
          Err(e) => todo!("malicious signer: {:?}", e),
        };

        // Get the machine, rebuilding it if we don't have it
        // We won't if the processor rebooted
        // This *may* be inconsistent if we receive a KeyGen for attempt x, then commitments for
        // attempt y
        // The coordinator is trusted to be proper in this regard
        let machine =
          self.active_commit.remove(&id.set).unwrap_or_else(|| key_gen_machine(id, params).0);

        let (machine, mut shares) =
          match machine.generate_secret_shares(&mut secret_shares_rng(id), parsed) {
            Ok(res) => res,
            Err(e) => todo!("malicious signer: {:?}", e),
          };
        self.active_share.insert(id.set, machine);

        let mut txn = self.db.0.txn();
        self.db.save_commitments(&mut txn, &id, &commitments);
        txn.commit();

        KeyGenEvent::ProcessorMessage(ProcessorMessage::Shares {
          id,
          shares: shares.drain().map(|(i, share)| (i, share.serialize())).collect(),
        })
      }

      CoordinatorMessage::Shares { id, mut shares } => {
        info!("Received shares for {:?}", id);

        let params = self.db.params(&id.set);

        // Parse the shares
        let shares = match shares
          .drain()
          .map(|(i, share)| {
            EncryptedMessage::<C::Curve, SecretShare<<C::Curve as Ciphersuite>::F>>::read::<&[u8]>(
              &mut share.as_ref(),
              params,
            )
            .map(|share| (i, share))
          })
          .collect()
        {
          Ok(shares) => shares,
          Err(e) => todo!("malicious signer: {:?}", e),
        };

        // Same commentary on inconsistency as above exists
        let machine = self.active_share.remove(&id.set).unwrap_or_else(|| {
          key_gen_machine(id, params)
            .0
            .generate_secret_shares(&mut secret_shares_rng(id), self.db.commitments(&id, params))
            .unwrap()
            .0
        });

        // TODO2: Handle the blame machine properly
        let keys = (match machine.calculate_share(&mut share_rng(id), shares) {
          Ok(res) => res,
          Err(e) => todo!("malicious signer: {:?}", e),
        })
        .complete();

        let mut txn = self.db.0.txn();
        self.db.save_keys(&mut txn, &id, &keys);
        txn.commit();

        let mut keys = ThresholdKeys::new(keys);
        C::tweak_keys(&mut keys);
        KeyGenEvent::ProcessorMessage(ProcessorMessage::GeneratedKey {
          id,
          key: keys.group_key().to_bytes().as_ref().to_vec(),
        })
      }

      CoordinatorMessage::ConfirmKey { context, id } => {
        let mut txn = self.db.0.txn();
        let keys = self.db.confirm_keys(&mut txn, &id);
        txn.commit();

        info!("Confirmed key {} from {:?}", hex::encode(keys.group_key().to_bytes()), id);

        KeyGenEvent::KeyConfirmed {
          activation_number: context.coin_latest_block_number.try_into().unwrap(),
          keys,
        }
      }
    }
  }
}