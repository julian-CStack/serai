use core::ops::Deref;
use std::collections::HashMap;

use zeroize::Zeroizing;

use ciphersuite::{Ciphersuite, Ristretto};

use tributary::{Signed, Block, TributaryReader};

use processor_messages::{
  key_gen::{self, KeyGenId},
  sign::{self, SignId},
  coordinator, CoordinatorMessage,
};

use serai_db::DbTxn;

use crate::{
  Db,
  processor::Processor,
  tributary::{TributaryDb, TributarySpec, Transaction},
};

// Handle a specific Tributary block
async fn handle_block<D: Db, Pro: Processor>(
  db: &mut TributaryDb<D>,
  key: &Zeroizing<<Ristretto as Ciphersuite>::F>,
  processor: &Pro,
  spec: &TributarySpec,
  block: Block<Transaction>,
) {
  let genesis = spec.genesis();
  let hash = block.hash();

  let mut event_id = 0;
  #[allow(clippy::explicit_counter_loop)] // event_id isn't TX index. It just currently lines up
  for tx in block.transactions {
    if !TributaryDb::<D>::handled_event(&db.0, hash, event_id) {
      let mut txn = db.0.txn();

      // Used to determine if an ID is acceptable
      #[derive(Clone, Copy, PartialEq, Eq, Debug)]
      enum Zone {
        Dkg,
        Batch,
        Sign,
      }

      impl Zone {
        fn label(&self) -> &'static str {
          match self {
            Zone::Dkg => {
              panic!("getting the label for dkg despite dkg code paths not needing a label")
            }
            Zone::Batch => "batch",
            Zone::Sign => "sign",
          }
        }
      }

      let mut handle = |zone: Zone,
                        label,
                        needed,
                        id,
                        attempt,
                        mut bytes: Vec<u8>,
                        signed: Signed| {
        if zone == Zone::Dkg {
          // Since Dkg doesn't have an ID, solely attempts, this should just be [0; 32]
          assert_eq!(id, [0; 32], "DKG, which shouldn't have IDs, had a non-0 ID");
        } else if !TributaryDb::<D>::recognized_id(&txn, zone.label(), genesis, id) {
          // TODO: Full slash
          todo!();
        }

        // If they've already published a TX for this attempt, slash
        if let Some(data) = TributaryDb::<D>::data(label, &txn, genesis, id, attempt, signed.signer)
        {
          if data != bytes {
            // TODO: Full slash
            todo!();
          }

          // TODO: Slash
          return None;
        }

        // If the attempt is lesser than the blockchain's, slash
        let curr_attempt = TributaryDb::<D>::attempt(&txn, genesis, id);
        if attempt < curr_attempt {
          // TODO: Slash for being late
          return None;
        }
        if attempt > curr_attempt {
          // TODO: Full slash
          todo!();
        }

        // TODO: We can also full slash if shares before all commitments, or share before the
        // necessary preprocesses

        // TODO: If this is shares, we need to check they are part of the selected signing set

        // Store this data
        let received =
          TributaryDb::<D>::set_data(label, &mut txn, genesis, id, attempt, signed.signer, &bytes);

        // If we have all the needed commitments/preprocesses/shares, tell the processor
        // TODO: This needs to be coded by weight, not by validator count
        if received == needed {
          let mut data = HashMap::new();
          for validator in spec.validators().iter().map(|validator| validator.0) {
            data.insert(
              spec.i(validator).unwrap(),
              if validator == signed.signer {
                bytes.split_off(0)
              } else if let Some(data) =
                TributaryDb::<D>::data(label, &txn, genesis, id, attempt, validator)
              {
                data
              } else {
                continue;
              },
            );
          }
          assert_eq!(data.len(), usize::from(needed));

          return Some(data);
        }
        None
      };

      match tx {
        Transaction::DkgCommitments(attempt, bytes, signed) => {
          if let Some(commitments) =
            handle(Zone::Dkg, b"dkg_commitments", spec.n(), [0; 32], attempt, bytes, signed)
          {
            processor
              .send(CoordinatorMessage::KeyGen(key_gen::CoordinatorMessage::Commitments {
                id: KeyGenId { set: spec.set(), attempt },
                commitments,
              }))
              .await;
          }
        }

        Transaction::DkgShares(attempt, mut shares, signed) => {
          if shares.len() != usize::from(spec.n()) {
            // TODO: Full slash
            todo!();
          }

          let bytes = shares
            .remove(
              &spec
                .i(Ristretto::generator() * key.deref())
                .expect("in a tributary we're not a validator for"),
            )
            .unwrap();

          if let Some(shares) =
            handle(Zone::Dkg, b"dkg_shares", spec.n(), [0; 32], attempt, bytes, signed)
          {
            processor
              .send(CoordinatorMessage::KeyGen(key_gen::CoordinatorMessage::Shares {
                id: KeyGenId { set: spec.set(), attempt },
                shares,
              }))
              .await;
          }
        }

        Transaction::ExternalBlock(block) => {
          // Because this external block has been finalized, its batch ID should be authorized

          // If we didn't provide this transaction, we should halt until we do
          // If we provided a distinct transaction, we should error
          // If we did provide this transaction, we should've set the batch ID for the block
          let batch_id = TributaryDb::<D>::batch_id(&txn, genesis, block).expect(
            "synced a tributary block finalizing a external block in a provided transaction \
            despite us not providing that transaction",
          );

          TributaryDb::<D>::recognize_id(&mut txn, Zone::Batch.label(), genesis, batch_id);
        }

        Transaction::SubstrateBlock(block) => {
          let plan_ids = TributaryDb::<D>::plan_ids(&txn, genesis, block).expect(
            "synced a tributary block finalizing a substrate block in a provided transaction \
            despite us not providing that transaction",
          );

          for id in plan_ids {
            TributaryDb::<D>::recognize_id(&mut txn, Zone::Sign.label(), genesis, id);
          }
        }

        Transaction::BatchPreprocess(data) => {
          if let Some(preprocesses) = handle(
            Zone::Batch,
            b"batch_preprocess",
            spec.t(),
            data.plan,
            data.attempt,
            data.data,
            data.signed,
          ) {
            processor
              .send(CoordinatorMessage::Coordinator(
                coordinator::CoordinatorMessage::BatchPreprocesses {
                  id: SignId { key: todo!(), id: data.plan, attempt: data.attempt },
                  preprocesses,
                },
              ))
              .await;
          }
        }
        Transaction::BatchShare(data) => {
          if let Some(shares) = handle(
            Zone::Batch,
            b"batch_share",
            spec.t(),
            data.plan,
            data.attempt,
            data.data,
            data.signed,
          ) {
            processor
              .send(CoordinatorMessage::Coordinator(coordinator::CoordinatorMessage::BatchShares {
                id: SignId { key: todo!(), id: data.plan, attempt: data.attempt },
                shares: shares
                  .drain()
                  .map(|(validator, share)| (validator, share.try_into().unwrap()))
                  .collect(),
              }))
              .await;
          }
        }

        Transaction::SignPreprocess(data) => {
          if let Some(preprocesses) = handle(
            Zone::Sign,
            b"sign_preprocess",
            spec.t(),
            data.plan,
            data.attempt,
            data.data,
            data.signed,
          ) {
            processor
              .send(CoordinatorMessage::Sign(sign::CoordinatorMessage::Preprocesses {
                id: SignId { key: todo!(), id: data.plan, attempt: data.attempt },
                preprocesses,
              }))
              .await;
          }
        }
        Transaction::SignShare(data) => {
          if let Some(shares) = handle(
            Zone::Sign,
            b"sign_share",
            spec.t(),
            data.plan,
            data.attempt,
            data.data,
            data.signed,
          ) {
            processor
              .send(CoordinatorMessage::Sign(sign::CoordinatorMessage::Shares {
                id: SignId { key: todo!(), id: data.plan, attempt: data.attempt },
                shares,
              }))
              .await;
          }
        }
      }

      TributaryDb::<D>::handle_event(&mut txn, hash, event_id);
      txn.commit();
    }
    event_id += 1;
  }

  // TODO: Trigger any necessary re-attempts
}

pub async fn handle_new_blocks<D: Db, Pro: Processor>(
  db: &mut TributaryDb<D>,
  key: &Zeroizing<<Ristretto as Ciphersuite>::F>,
  processor: &Pro,
  spec: &TributarySpec,
  tributary: &TributaryReader<D, Transaction>,
) {
  let genesis = tributary.genesis();
  let mut last_block = db.last_block(genesis);
  while let Some(next) = tributary.block_after(&last_block) {
    let block = tributary.block(&next).unwrap();
    handle_block(db, key, processor, spec, block).await;
    last_block = next;
    db.set_last_block(genesis, next);
  }
}
