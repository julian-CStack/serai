use std::collections::{HashSet, HashMap, BTreeMap};

use zeroize::{Zeroize, ZeroizeOnDrop};
use rand_core::{RngCore, CryptoRng};

use transcript::Transcript;
use ciphersuite::{
  group::{ff::Field, GroupEncoding},
  Ciphersuite,
};

use crate::{
  ScalarVector, ScalarMatrix, PointVector, weighted_inner_product::*, arithmetic_circuit_proof,
};
pub use arithmetic_circuit_proof::*;

#[allow(non_snake_case)]
#[derive(Clone, PartialEq, Eq, Debug, Zeroize, ZeroizeOnDrop)]
pub struct Commitment<C: Ciphersuite> {
  pub value: C::F,
  pub mask: C::F,
}

impl<C: Ciphersuite> Commitment<C> {
  pub fn zero() -> Self {
    Commitment { value: C::F::ZERO, mask: C::F::ZERO }
  }

  pub fn new(value: C::F, mask: C::F) -> Self {
    Commitment { value, mask }
  }

  pub fn masking<R: RngCore + CryptoRng>(rng: &mut R, value: C::F) -> Self {
    Commitment { value, mask: C::F::random(rng) }
  }

  /// Calculate a Pedersen commitment, as a point, from the transparent structure.
  pub fn calculate(&self, g: C::G, h: C::G) -> C::G {
    (g * self.value) + (h * self.mask)
  }
}

#[derive(Clone, Debug, Zeroize, ZeroizeOnDrop)]
pub enum Variable<C: Ciphersuite> {
  Secret(Option<C::F>),
  Committed(Option<Commitment<C>>, C::G),
  Product(usize, Option<C::F>),
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Zeroize)]
pub struct VariableReference(usize);
// TODO: Remove Ord and usage of HashMaps/BTreeMaps
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Zeroize)]
pub enum ProductReference {
  Left { product: usize, variable: usize },
  Right { product: usize, variable: usize },
  Output { product: usize, variable: usize },
}
#[derive(Copy, Clone, Debug, Zeroize)]
pub struct CommitmentReference(usize);
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Zeroize)]
pub struct VectorCommitmentReference(usize);

#[derive(Clone, Debug)]
pub struct Constraint<C: Ciphersuite> {
  label: &'static str,
  // Each weight (C::F) is bound to a specific variable (usize) to allow post-expansion to valid
  // constraints
  WL: Vec<(usize, C::F)>,
  WR: Vec<(usize, C::F)>,
  WO: Vec<(usize, C::F)>,
  WV: Vec<(usize, C::F)>,
  c: C::F,
}

impl<C: Ciphersuite> Constraint<C> {
  pub fn new(label: &'static str) -> Self {
    Self { label, WL: vec![], WR: vec![], WO: vec![], WV: vec![], c: C::F::ZERO }
  }

  pub fn weight(&mut self, product: ProductReference, weight: C::F) -> &mut Self {
    let (weights, id) = match product {
      ProductReference::Left { product: id, variable: _ } => (&mut self.WL, id),
      ProductReference::Right { product: id, variable: _ } => (&mut self.WR, id),
      ProductReference::Output { product: id, variable: _ } => (&mut self.WO, id),
    };
    for existing in &mut *weights {
      if existing.0 == id {
        existing.1 += weight;
        return self;
      }
    }
    weights.push((id, weight));
    self
  }
  pub fn weight_commitment(&mut self, variable: CommitmentReference, weight: C::F) -> &mut Self {
    for existing in &self.WV {
      assert!(existing.0 != variable.0);
    }
    self.WV.push((variable.0, weight));
    self
  }
  pub fn rhs_offset(&mut self, offset: C::F) -> &mut Self {
    assert!(bool::from(self.c.is_zero()));
    self.c = offset;
    self
  }
}

impl<C: Ciphersuite> Variable<C> {
  pub fn value(&self) -> Option<C::F> {
    match self {
      Variable::Secret(value) => *value,
      // This branch should never be reachable due to usage of CommitmentReference
      Variable::Committed(_commitment, _) => {
        // commitment.map(|commitment| commitment.value),
        panic!("requested value of commitment");
      }
      Variable::Product(_, product) => *product,
    }
  }
}

#[derive(Clone, PartialEq, Eq, Debug, Zeroize)]
struct Product {
  left: usize,
  right: usize,
  variable: usize,
}

pub struct Circuit<C: Ciphersuite> {
  g: C::G,
  h: C::G,
  g_bold1: PointVector<C>,
  g_bold2: PointVector<C>,
  h_bold1: PointVector<C>,
  h_bold2: PointVector<C>,

  prover: bool,

  commitments: usize,
  pub(crate) variables: Vec<Variable<C>>,

  products: Vec<Product>,
  bound_products: Vec<BTreeMap<ProductReference, Option<C::G>>>,
  finalized_commitments: HashMap<VectorCommitmentReference, Option<C::F>>,
  vector_commitments: Option<Vec<C::G>>,

  constraints: Vec<Constraint<C>>,
}

impl<C: Ciphersuite> Circuit<C> {
  pub fn new(
    g: C::G,
    h: C::G,
    g_bold1: PointVector<C>,
    g_bold2: PointVector<C>,
    h_bold1: PointVector<C>,
    h_bold2: PointVector<C>,
    prover: bool,
    vector_commitments: Option<Vec<C::G>>,
  ) -> Self {
    assert_eq!(prover, vector_commitments.is_none());

    Self {
      g,
      h,
      g_bold1,
      g_bold2,
      h_bold1,
      h_bold2,

      prover,

      commitments: 0,
      variables: vec![],

      products: vec![],
      bound_products: vec![],
      finalized_commitments: HashMap::new(),
      vector_commitments,

      constraints: vec![],
    }
  }

  pub fn prover(&self) -> bool {
    self.prover
  }

  pub fn h(&self) -> C::G {
    self.h
  }

  /// Obtain the underlying value from a variable reference.
  pub fn unchecked_value(&self, variable: VariableReference) -> Option<C::F> {
    self.variables[variable.0].value()
  }

  pub fn variable(&self, product: ProductReference) -> VariableReference {
    match product {
      ProductReference::Left { variable, .. } => VariableReference(variable),
      ProductReference::Right { variable, .. } => VariableReference(variable),
      ProductReference::Output { variable, .. } => VariableReference(variable),
    }
  }

  pub fn variable_to_product(&self, variable: VariableReference) -> Option<ProductReference> {
    if let Variable::Product(product, _) = self.variables[variable.0] {
      return Some(ProductReference::Output { product, variable: variable.0 });
    }

    for (product_id, product) in self.products.iter().enumerate() {
      let Product { left: l, right: r, variable: this_variable } = product;

      if !((variable.0 == *l) || (variable.0 == *r)) {
        continue;
      }

      if let Variable::Product(var_product_id, _) = self.variables[*this_variable] {
        debug_assert_eq!(var_product_id, product_id);
        if variable.0 == *l {
          return Some(ProductReference::Left {
            product: product_id,
            variable: self.products[var_product_id].left,
          });
        } else {
          return Some(ProductReference::Right {
            product: product_id,
            variable: self.products[var_product_id].right,
          });
        }
      } else {
        panic!("product pointed to non-product variable");
      }
    }

    None
  }

  /// Use a pair of variables in a product relationship.
  pub fn product(
    &mut self,
    a: VariableReference,
    b: VariableReference,
  ) -> ((ProductReference, ProductReference, ProductReference), VariableReference) {
    for (id, product) in self.products.iter().enumerate() {
      if (a.0 == product.left) && (b.0 == product.right) {
        return (
          (
            ProductReference::Left { product: id, variable: a.0 },
            ProductReference::Right { product: id, variable: b.0 },
            ProductReference::Output { product: id, variable: product.variable },
          ),
          VariableReference(product.variable),
        );
      }
    }

    let existing_a_use = self.variable_to_product(a);
    let existing_b_use = self.variable_to_product(b);

    let left = &self.variables[a.0];
    let right = &self.variables[b.0];

    let product_id = self.products.len();
    let variable = VariableReference(self.variables.len());
    let products = (
      ProductReference::Left { product: product_id, variable: a.0 },
      ProductReference::Right { product: product_id, variable: b.0 },
      ProductReference::Output { product: product_id, variable: variable.0 },
    );

    self.products.push(Product { left: a.0, right: b.0, variable: variable.0 });
    self.variables.push(Variable::Product(
      product_id,
      Some(()).filter(|_| self.prover).map(|_| left.value().unwrap() * right.value().unwrap()),
    ));

    // Add consistency constraints with prior variable uses
    if let Some(existing) = existing_a_use {
      self.constrain_equality(products.0, existing);
    }
    if let Some(existing) = existing_b_use {
      self.constrain_equality(products.1, existing);
    }

    (products, variable)
  }

  /// Add an input only known to the prover.
  pub fn add_secret_input(&mut self, value: Option<C::F>) -> VariableReference {
    assert_eq!(self.prover, value.is_some());

    let res = VariableReference(self.variables.len());
    self.variables.push(Variable::Secret(value));
    res
  }

  /// Add an input publicly committed to.
  pub fn add_committed_input(
    &mut self,
    commitment: Option<Commitment<C>>,
    actual: C::G,
  ) -> CommitmentReference {
    assert_eq!(self.prover, commitment.is_some());
    if let Some(commitment) = commitment.clone() {
      assert_eq!(commitment.calculate(self.g, self.h), actual);
    }

    let res = CommitmentReference(self.commitments);
    self.commitments += 1;
    self.variables.push(Variable::Committed(commitment, actual));
    res
  }

  /// Add a constraint.
  pub fn constrain(&mut self, constraint: Constraint<C>) {
    self.constraints.push(constraint);
  }

  pub fn constrain_equality(&mut self, a: ProductReference, b: ProductReference) {
    if a == b {
      return;
    }

    let mut constraint = Constraint::new("equality");
    constraint.weight(a, C::F::ONE);
    constraint.weight(b, -C::F::ONE);
    self.constrain(constraint);
  }

  pub fn equals_constant(&mut self, a: ProductReference, b: C::F) {
    let mut constraint = Constraint::new("constant_equality");
    if b == C::F::ZERO {
      constraint.weight(a, C::F::ONE);
    } else {
      constraint.weight(a, b.invert().unwrap());
      constraint.rhs_offset(C::F::ONE);
    }
    self.constrain(constraint);
  }

  /// Allocate a vector commitment ID.
  pub fn allocate_vector_commitment(&mut self) -> VectorCommitmentReference {
    let res = VectorCommitmentReference(self.bound_products.len());
    self.bound_products.push(BTreeMap::new());
    res
  }

  /// Bind a product variable into a vector commitment, using the specified generator.
  ///
  /// If no generator is specified, the proof's existing generator will be used. This allows
  /// isolating the variable, prior to the circuit, without caring for how it was isolated.
  pub fn bind(
    &mut self,
    vector_commitment: VectorCommitmentReference,
    product: ProductReference,
    generator: Option<C::G>,
  ) {
    assert!(!self.finalized_commitments.contains_key(&vector_commitment));

    // TODO: Check generators are unique (likely best done in the Wip itself)
    for bound in &self.bound_products {
      assert!(!bound.contains_key(&product));
    }
    self.bound_products[vector_commitment.0].insert(product, generator);
  }

  /// Finalize a vector commitment, returning it, preventing further binding.
  pub fn finalize_commitment(
    &mut self,
    vector_commitment: VectorCommitmentReference,
    blind: Option<C::F>,
  ) -> C::G {
    if self.prover() {
      // Calculate and return the vector commitment
      // TODO: Use a multiexp here
      let mut commitment = self.h * blind.unwrap();
      for (product, generator) in self.bound_products[vector_commitment.0].clone() {
        commitment += match product {
          ProductReference::Left { product, variable } => {
            generator.unwrap_or(self.g_bold1[product]) * self.variables[variable].value().unwrap()
          }
          ProductReference::Right { product, variable } => {
            generator.unwrap_or(self.h_bold1[product]) * self.variables[variable].value().unwrap()
          }
          ProductReference::Output { product, variable } => {
            generator.unwrap_or(self.g_bold2[product]) * self.variables[variable].value().unwrap()
          }
        };
      }
      self.finalized_commitments.insert(vector_commitment, blind);
      commitment
    } else {
      assert!(blind.is_none());
      self.finalized_commitments.insert(vector_commitment, None);
      self.vector_commitments.as_ref().unwrap()[vector_commitment.0]
    }
  }

  // TODO: This can be optimized with post-processing passes
  // TODO: Don't run this on every single prove/verify. It should only be run once at compile time
  fn compile(
    mut self,
  ) -> (
    ArithmeticCircuitStatement<C>,
    Vec<Vec<(Option<C::F>, C::G)>>,
    Vec<(Option<C::F>, C::G)>,
    Option<ArithmeticCircuitWitness<C>>,
  ) {
    let witness = if self.prover {
      let mut aL = vec![];
      let mut aR = vec![];

      let mut v = vec![];
      let mut gamma = vec![];

      for variable in &self.variables {
        match variable {
          Variable::Secret(_) => {}
          Variable::Committed(value, actual) => {
            let value = value.as_ref().unwrap();
            assert_eq!(value.calculate(self.g, self.h), *actual);
            v.push(value.value);
            gamma.push(value.mask);
          }
          Variable::Product(product_id, _) => {
            let product = &self.products[*product_id];
            aL.push(self.variables[product.left].value().unwrap());
            aR.push(self.variables[product.right].value().unwrap());
          }
        }
      }

      Some(ArithmeticCircuitWitness::new(
        ScalarVector(aL),
        ScalarVector(aR),
        ScalarVector(v),
        ScalarVector(gamma),
      ))
    } else {
      None
    };

    let mut V = vec![];
    let mut n = 0;
    for variable in &self.variables {
      match variable {
        Variable::Secret(_) => {}
        Variable::Committed(_, actual) => V.push(*actual),
        Variable::Product(_, _) => n += 1,
      }
    }

    // WL, WR, WO, WV, c
    let mut WL = ScalarMatrix::new(n);
    let mut WR = ScalarMatrix::new(n);
    let mut WO = ScalarMatrix::new(n);
    let mut WV = ScalarMatrix::new(V.len());
    let mut c = vec![];

    for constraint in self.constraints {
      // WL aL WR aR WO aO == WV v + c
      let mut eval = C::F::ZERO;

      let mut this_wl = vec![];
      let mut this_wr = vec![];
      let mut this_wo = vec![];
      let mut this_wv = vec![];

      for wl in constraint.WL {
        if self.prover {
          eval += wl.1 * witness.as_ref().unwrap().aL[wl.0];
        }
        this_wl.push(wl);
      }
      for wr in constraint.WR {
        if self.prover {
          eval += wr.1 * witness.as_ref().unwrap().aR[wr.0];
        }
        this_wr.push(wr);
      }
      for wo in constraint.WO {
        if self.prover {
          eval += wo.1 * (witness.as_ref().unwrap().aL[wo.0] * witness.as_ref().unwrap().aR[wo.0]);
        }
        this_wo.push(wo);
      }
      for wv in constraint.WV {
        if self.prover {
          eval -= wv.1 * witness.as_ref().unwrap().v[wv.0];
        }
        this_wv.push(wv);
      }

      if self.prover {
        assert_eq!(eval, constraint.c, "faulty constraint: {}", constraint.label);
      }

      WL.push(this_wl);
      WR.push(this_wr);
      WO.push(this_wo);
      WV.push(this_wv);
      c.push(constraint.c);
    }

    // The A commitment is g1 aL, g2 aO, h1 aR
    // Override the generators used for these products, if they were bound to a specific generator
    // Also tracks the variables relevant to vector commitments and the variables not
    let mut vc_used = HashSet::new();
    let mut vector_commitments = vec![vec![]; self.bound_products.len()];
    let mut others = vec![];
    for (vc, bindings) in self.bound_products.iter().enumerate() {
      for (product, g) in bindings {
        let g = *g;
        match *product {
          ProductReference::Left { product, .. } => {
            let g = g.unwrap_or(self.g_bold1[product]);
            self.g_bold1[product] = g;
            vc_used.insert(('l', product));
            vector_commitments[vc].push((witness.as_ref().map(|witness| witness.aL[product]), g));
          }
          ProductReference::Right { product, .. } => {
            let g = g.unwrap_or(self.h_bold1[product]);
            self.h_bold1[product] = g;
            vc_used.insert(('r', product));
            vector_commitments[vc].push((witness.as_ref().map(|witness| witness.aR[product]), g));
          }
          ProductReference::Output { product, .. } => {
            let g = g.unwrap_or(self.g_bold2[product]);
            self.g_bold2[product] = g;
            vc_used.insert(('o', product));
            vector_commitments[vc]
              .push((witness.as_ref().map(|witness| witness.aL[product] * witness.aR[product]), g));
          }
        }
      }
    }

    fn add_to_others<C: Ciphersuite, I: Iterator<Item = Option<C::F>>>(
      label: char,
      vars: I,
      gens: &[C::G],
      vc_used: &HashSet<(char, usize)>,
      others: &mut Vec<(Option<C::F>, C::G)>,
    ) {
      for (p, var) in vars.enumerate() {
        if !vc_used.contains(&(label, p)) {
          others.push((var, gens[p]));
        }
      }
    }
    add_to_others::<C, _>(
      'l',
      (0 .. self.products.len()).map(|i| witness.as_ref().map(|witness| witness.aL[i])),
      &self.g_bold1.0,
      &vc_used,
      &mut others,
    );
    add_to_others::<C, _>(
      'r',
      (0 .. self.products.len()).map(|i| witness.as_ref().map(|witness| witness.aR[i])),
      &self.h_bold1.0,
      &vc_used,
      &mut others,
    );
    add_to_others::<C, _>(
      'o',
      (0 .. self.products.len())
        .map(|i| witness.as_ref().map(|witness| witness.aL[i] * witness.aR[i])),
      &self.g_bold2.0,
      &vc_used,
      &mut others,
    );

    (
      ArithmeticCircuitStatement::new(
        self.g,
        self.h,
        self.g_bold1,
        self.g_bold2,
        self.h_bold1,
        self.h_bold2,
        PointVector(V),
        WL,
        WR,
        WO,
        WV,
        ScalarVector(c),
      ),
      vector_commitments,
      others,
      witness,
    )
  }

  pub fn prove<R: RngCore + CryptoRng, T: Transcript>(
    self,
    rng: &mut R,
    transcript: &mut T,
  ) -> ArithmeticCircuitProof<C> {
    assert!(self.prover);
    let (statement, vector_commitments, _, witness) = self.compile();
    assert!(vector_commitments.is_empty());
    statement.prove(rng, transcript, witness.unwrap())
  }

  fn vector_commitment_statement<T: Transcript>(
    g: C::G,
    hs: &[C::G],
    transcript: &mut T,
    generators: Vec<C::G>,
    H: C::G,
    commitment: C::G,
  ) -> (WipStatement<C>, C::F) {
    transcript.append_message(b"vector_commitment", commitment.to_bytes());

    // TODO: Do we need to transcript more before this? Should we?
    let y = C::hash_to_F(b"vector_commitment_proof", transcript.challenge(b"y").as_ref());

    let generators_len = generators.len();
    // TODO: Why isn't y in the statement?
    (
      WipStatement::new(
        g,
        H,
        PointVector(generators),
        PointVector(hs[.. generators_len].to_vec()),
        commitment,
      ),
      y,
    )
  }

  pub fn verify<T: Transcript>(self, transcript: &mut T, proof: ArithmeticCircuitProof<C>) {
    assert!(!self.prover);
    assert!(self.vector_commitments.as_ref().unwrap().is_empty());
    let (statement, vector_commitments, _, _) = self.compile();
    assert!(vector_commitments.is_empty());
    statement.verify(transcript, proof)
  }

  // Returns the blinds used, the blinded vector commitments, the proof, and proofs the vector
  // commitments are well formed
  // TODO: Create a dedicated struct for this return value
  pub fn prove_with_vector_commitments<R: RngCore + CryptoRng, T: Transcript>(
    self,
    rng: &mut R,
    transcript: &mut T,
    additional_proving_gs: (C::G, C::G),
    additional_proving_hs: (Vec<C::G>, Vec<C::G>),
  ) -> (Vec<C::F>, Vec<C::G>, ArithmeticCircuitProof<C>, Vec<(WipProof<C>, WipProof<C>)>) {
    assert!(self.prover);

    let finalized_commitments = self.finalized_commitments.clone();
    let (statement, mut vector_commitments, others, witness) = self.compile();
    assert!(!vector_commitments.is_empty());
    let witness = witness.unwrap();

    /*
      In lieu of a proper vector commitment scheme, the following is done.

      The arithmetic circuit proof takes in a commitment of all product statements.
      That commitment is of the form left G1, right H1, out G2.

      Each vector commitment is for a series of variables against specfic generators.

      For each required vector commitment, a proof of a known DLog for the commitment, against the
      specified generators, is provided via a pair of WIP proofs.

      Finally, another pair of WIP proofs proves a known DLog for the remaining generators in this
      arithmetic circuit proof.

      The arithmetic circuit's in-proof commitment is then defined as the sum of the commitments
      and the commitment to the remaining variables.

      This forces the commitment to commit as the vector commitments do.

      The security of this is assumed. Technically, the commitment being well-formed isn't
      guaranteed by the Weighted Inner Product relationship. A formal proof of the security of this
      requires that property being proven. Such a proof may already exist as part of the WIP proof.

      TODO

      As one other note, a single WIP proof is likely fine, with parallelized g_bold/h_bold, if the
      prover provides the G component and a Schnorr PoK for it. While they may lie, leaving the G
      component, that shouldn't create any issues so long as G is distinct for all such proofs.

      That wasn't done here as it further complicates a complicated enough already scheme.
    */

    fn well_formed<R: RngCore + CryptoRng, C: Ciphersuite, T: Transcript>(
      rng: &mut R,
      additional_gs: (C::G, C::G),
      additional_hs: &(Vec<C::G>, Vec<C::G>),
      transcript: &mut T,
      scalars: Vec<C::F>,
      generators: Vec<C::G>,
      blind: C::F,
      H: C::G,
    ) -> (C::G, (WipProof<C>, WipProof<C>)) {
      // TODO: Use a multiexp here
      let mut commitment = H * blind;
      for (scalar, generator) in scalars.iter().zip(generators.iter()) {
        commitment += *generator * scalar;
      }

      let b = ScalarVector(vec![C::F::ZERO; scalars.len()]);
      let witness = WipWitness::<C>::new(ScalarVector(scalars), b, blind);

      (
        commitment,
        (
          {
            let (statement, y) = Circuit::<C>::vector_commitment_statement(
              additional_gs.0,
              &additional_hs.0,
              transcript,
              generators.clone(),
              H,
              commitment,
            );
            let mut t_c = transcript.clone();
            let proof = statement.clone().prove(&mut *rng, transcript, witness.clone(), y);
            statement.verify(&mut t_c, proof.clone(), y);
            proof
          },
          {
            let (statement, y) = Circuit::<C>::vector_commitment_statement(
              additional_gs.1,
              &additional_hs.1,
              transcript,
              generators,
              H,
              commitment,
            );
            statement.prove(&mut *rng, transcript, witness, y)
          },
        ),
      )
    }

    let mut blinds = vec![];
    let mut commitments = vec![];
    let mut proofs = vec![];
    for (vc, vector_commitment) in vector_commitments.drain(..).enumerate() {
      let mut scalars = vec![];
      let mut generators = vec![];
      for (var, point) in vector_commitment {
        scalars.push(var.unwrap());
        generators.push(point);
      }
      blinds.push(
        finalized_commitments
          .get(&VectorCommitmentReference(vc))
          .cloned()
          .unwrap_or(Some(C::F::random(&mut *rng)))
          .unwrap(),
      );

      let (commitment, proof) = well_formed::<_, C, _>(
        &mut *rng,
        additional_proving_gs,
        &additional_proving_hs,
        transcript,
        scalars,
        generators,
        blinds[blinds.len() - 1],
        statement.h,
      );
      commitments.push(commitment);
      proofs.push(proof);
    }
    let vector_commitments = commitments;

    // Push one final WIP proof for all other variables
    let other_commitment;
    let other_blind = C::F::random(&mut *rng);
    {
      let mut scalars = vec![];
      let mut generators = vec![];
      for (scalar, generator) in others {
        scalars.push(scalar.unwrap());
        generators.push(generator);
      }
      let proof;
      (other_commitment, proof) = well_formed::<_, C, _>(
        &mut *rng,
        additional_proving_gs,
        &additional_proving_hs,
        transcript,
        scalars,
        generators,
        other_blind,
        statement.h,
      );
      proofs.push(proof);
    }

    let proof = statement.prove_with_blind(
      rng,
      transcript,
      witness,
      blinds.iter().sum::<C::F>() + other_blind,
    );
    debug_assert_eq!(proof.A, vector_commitments.iter().sum::<C::G>() + other_commitment);

    (blinds, vector_commitments, proof, proofs)
  }

  pub fn verify_with_vector_commitments<T: Transcript>(
    self,
    transcript: &mut T,
    additional_proving_gs: (C::G, C::G),
    additional_proving_hs: (Vec<C::G>, Vec<C::G>),
    proof: ArithmeticCircuitProof<C>,
    mut vc_proofs: Vec<(WipProof<C>, WipProof<C>)>,
  ) {
    assert!(!self.prover);
    let vector_commitments = self.vector_commitments.clone().unwrap();
    let (statement, mut vector_commitments_data, mut others, _) = self.compile();
    assert_eq!(vector_commitments.len(), vector_commitments_data.len());

    let mut verify_proofs = |generators: Vec<_>, commitment, proofs: (_, _)| {
      let (wip_statement, y) = Self::vector_commitment_statement(
        additional_proving_gs.0,
        &additional_proving_hs.0,
        transcript,
        generators.clone(),
        statement.h,
        commitment,
      );
      wip_statement.verify(transcript, proofs.0, y);

      let (wip_statement, y) = Self::vector_commitment_statement(
        additional_proving_gs.1,
        &additional_proving_hs.1,
        transcript,
        generators,
        statement.h,
        commitment,
      );
      wip_statement.verify(transcript, proofs.1, y);
    };

    assert_eq!(vector_commitments.len() + 1, vc_proofs.len());
    for ((commitment, mut data), proofs) in vector_commitments
      .iter()
      .zip(vector_commitments_data.drain(..))
      .zip(vc_proofs.drain(.. (vc_proofs.len() - 1)))
    {
      verify_proofs(data.drain(..).map(|(_, g)| g).collect(), *commitment, proofs);
    }

    {
      assert_eq!(vc_proofs.len(), 1);
      verify_proofs(
        others.drain(..).map(|(_, g)| g).collect(),
        proof.A - vector_commitments.iter().sum::<C::G>(),
        vc_proofs.swap_remove(0),
      );
    }

    statement.verify(transcript, proof)
  }
}