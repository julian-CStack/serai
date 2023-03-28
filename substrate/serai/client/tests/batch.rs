use rand_core::{RngCore, OsRng};

use sp_core::sr25519::Signature;

use serai_client::{
  primitives::{BITCOIN_NET_ID, BITCOIN, BlockHash, SeraiAddress, Amount, Balance},
  tokens::TokensEvent,
  in_instructions::{
    primitives::{InInstruction, InInstructionWithBalance, Batch, SignedBatch},
    InInstructionsEvent,
  },
  Serai,
};

mod runner;
use runner::{URL, provide_batch};

serai_test!(
  async fn publish_batch() {
    let network = BITCOIN_NET_ID;
    let id = 0;

    let mut block_hash = BlockHash([0; 32]);
    OsRng.fill_bytes(&mut block_hash.0);

    let mut address = SeraiAddress::new([0; 32]);
    OsRng.fill_bytes(&mut address.0);

    let coin = BITCOIN;
    let amount = Amount(OsRng.next_u64().saturating_add(1));
    let balance = Balance { coin, amount };

    let batch = Batch {
      network,
      id,
      block: block_hash,
      instructions: vec![InInstructionWithBalance {
        instruction: InInstruction::Transfer(address),
        balance,
      }],
    };
    let signed = SignedBatch { batch, signature: Signature::from_raw([0; 64]) };
    let block = provide_batch(signed).await;

    let serai = Serai::new(URL).await.unwrap();
    let batches = serai.get_batch_events(block).await.unwrap();
    assert_eq!(batches, vec![InInstructionsEvent::Batch { network, id, block: block_hash }]);

    assert_eq!(
      serai.get_mint_events(block).await.unwrap(),
      vec![TokensEvent::Mint { address, balance }],
    );
    assert_eq!(serai.get_token_supply(block, coin).await.unwrap(), amount);
    assert_eq!(serai.get_token_balance(block, coin, address).await.unwrap(), amount);
  }
);