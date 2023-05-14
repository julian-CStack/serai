#![allow(non_snake_case)]

use zeroize::{Zeroize, ZeroizeOnDrop};

use ciphersuite::{group::ff::Field, Ciphersuite};

pub mod scalar_vector;
pub(crate) use scalar_vector::{ScalarVector, weighted_inner_product};
pub mod point_vector;
pub(crate) use point_vector::PointVector;

pub mod weighted_inner_product;

#[cfg(test)]
mod tests;

pub trait BulletproofsCurve: Ciphersuite {
  fn alt_generator() -> <Self as Ciphersuite>::G;
  fn alt_generators() -> &'static [<Self as Ciphersuite>::G];
}

#[allow(non_snake_case)]
#[derive(Clone, PartialEq, Eq, Debug, Zeroize, ZeroizeOnDrop)]
pub struct Commitment<C: Ciphersuite> {
  pub mask: C::F,
  pub value: u64,
}

impl<C: BulletproofsCurve> Commitment<C> {
  pub fn zero() -> Self {
    Commitment { mask: C::F::ZERO, value: 0 }
  }

  pub fn new(mask: C::F, value: u64) -> Self {
    Commitment { mask, value }
  }

  /// Calculate a Pedersen commitment, as a point, from the transparent structure.
  pub fn calculate(&self) -> C::G {
    (C::generator() * self.mask) + (C::alt_generator() * C::F::from(self.value))
  }
}