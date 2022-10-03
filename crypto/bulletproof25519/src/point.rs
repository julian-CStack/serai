use core::{
  ops::{Add, AddAssign, Neg, Sub, SubAssign, Mul, MulAssign},
  iter::Sum,
};

use lazy_static::lazy_static;

use rand_core::RngCore;

use zeroize::Zeroize;
use subtle::{Choice, CtOption, ConstantTimeEq, ConditionallySelectable, ConditionallyNegatable};

use crypto_bigint::U512;

use ff::{Field, PrimeField, PrimeFieldBits};
use group::{Group, GroupEncoding, prime::PrimeGroup};

use crate::{
  scalar::{Scalar, ORDER},
  field::FieldElement,
};

// Generator X coordinate created via
// sha512("Bulletproof25519 Generator") =
// 24160648779d1b6e09a632ee5665113f0f47c859f39f806cb4e89e7f6e4de1c2
// 521ff50761c9de5d7242a79fb00611cdb4993d2c6c795f81851d2cc85194b38b
// The first 33 bytes were grabbed, and the first 4 bits were dropped
// This remains completely unbiased since the first 4 bits are never used
const G_X: FieldElement = FieldElement(U512::from_be_hex(concat!(
  "00000000000000000000000000000000000000000000000000000000000000",
  "04160648779d1b6e09a632ee5665113f0f47c859f39f806cb4e89e7f6e4de1c252",
)));

fn recover_y(x: FieldElement) -> CtOption<FieldElement> {
  let y = ((x.square() * x) + FieldElement::one()).sqrt().unwrap();
  CtOption::new(y, ((x.square() * x) + FieldElement::one()).ct_eq(&y.square()))
}

#[derive(Clone, Copy, Debug, Zeroize)]
pub struct Point {
  x: FieldElement,
  y: FieldElement,
  z: FieldElement,
}

lazy_static! {
  static ref G: Point =
    (Point { x: G_X, y: recover_y(G_X).unwrap(), z: FieldElement::one() }).mul_by_cofactor();
}

impl ConstantTimeEq for Point {
  fn ct_eq(&self, other: &Self) -> Choice {
    let z1_2 = self.z.square();
    let z2_2 = other.z.square();
    let u1 = self.x * z2_2;
    let u2 = other.x * z1_2;
    let s1 = self.y * other.z * z2_2;
    let s2 = other.y * self.z * z1_2;
    u1.ct_eq(&u2) & s1.ct_eq(&s2)
  }
}

impl PartialEq for Point {
  fn eq(&self, other: &Point) -> bool {
    self.ct_eq(other).into()
  }
}

impl Eq for Point {}

impl ConditionallySelectable for Point {
  fn conditional_select(a: &Self, b: &Self, choice: Choice) -> Self {
    Point {
      x: FieldElement::conditional_select(&a.x, &b.x, choice),
      y: FieldElement::conditional_select(&a.y, &b.y, choice),
      z: FieldElement::conditional_select(&a.z, &b.z, choice),
    }
  }
}

impl Add for Point {
  type Output = Point;
  fn add(self, other: Self) -> Self {
    // Return the point which isn't identity if one is
    let res = CtOption::new(self, other.x.is_zero());
    let res = res.or_else(|| CtOption::new(other, self.x.is_zero()));

    // Variables needed for both addition and equality checking
    let z1_2 = self.z.square();
    let z2_2 = other.z.square();

    let u1 = self.x * z2_2;
    let u2 = other.x * z1_2;
    let s1 = self.y * other.z * z2_2;
    let s2 = other.y * self.z * z1_2;

    // Return double if they're equal
    let eq = u1.ct_eq(&u2) & s1.ct_eq(&s2);
    let double = self.double();
    let res = res.or_else(|| CtOption::new(double, eq));

    // Return identity if other == -self
    let neg_eq = u1.ct_eq(&u2) & s1.ct_eq(&-s2);
    let res = res.or_else(|| CtOption::new(Point::identity(), neg_eq));

    // Finish the addition
    // add-2007-bl
    let h = u2 - u1;

    let i = h.double().square();
    let j = h * i;
    let r = (s2 - s1).double();
    let v = u1 * i;

    let x = r.square() - j - v.double();
    let candidate = Point {
      x,
      y: (r * (v - x)) - (s1.double() * j),
      z: ((self.z + other.z).square() - z1_2 - z2_2) * h,
    };

    let res = res.or_else(|| CtOption::new(candidate, 1.into())).unwrap();

    // TODO: Is this proper? It's a mirror of the first check
    Point::conditional_select(&res, &Point::identity(), res.x.is_zero())
  }
}

impl AddAssign for Point {
  fn add_assign(&mut self, other: Point) {
    *self = *self + other;
  }
}

impl Add<&Point> for Point {
  type Output = Point;
  fn add(self, other: &Point) -> Point {
    self + *other
  }
}

impl AddAssign<&Point> for Point {
  fn add_assign(&mut self, other: &Point) {
    *self += *other;
  }
}

impl Neg for Point {
  type Output = Point;
  fn neg(self) -> Self {
    Point { x: self.x, y: -self.y, z: self.z }
  }
}

impl Sub for Point {
  type Output = Point;
  #[allow(clippy::suspicious_arithmetic_impl)]
  fn sub(self, other: Self) -> Self {
    self + other.neg()
  }
}

impl SubAssign for Point {
  fn sub_assign(&mut self, other: Point) {
    *self = *self - other;
  }
}

impl Sub<&Point> for Point {
  type Output = Point;
  fn sub(self, other: &Point) -> Point {
    self - *other
  }
}

impl SubAssign<&Point> for Point {
  fn sub_assign(&mut self, other: &Point) {
    *self -= *other;
  }
}

impl Group for Point {
  type Scalar = Scalar;
  fn random(mut rng: impl RngCore) -> Self {
    loop {
      let mut bytes = FieldElement::random(&mut rng).to_repr();
      let mut_ref: &mut [u8] = bytes.as_mut();
      mut_ref[33] |= u8::try_from(rng.next_u32() % 2).unwrap() << 7;
      let opt = Self::from_bytes(&bytes);
      if opt.is_some().into() {
        return opt.unwrap();
      }
    }
  }
  fn identity() -> Self {
    Point { x: FieldElement::zero(), y: FieldElement::one(), z: FieldElement::zero() }
  }
  fn generator() -> Self {
    *G
  }
  fn is_identity(&self) -> Choice {
    self.ct_eq(&Self::identity())
  }
  fn double(&self) -> Self {
    let a = self.x.square();
    let b = self.y.square();
    let c = b.square();
    let d = ((self.x + b).square() - a - c).double();
    let e = a.double() + a;
    let f = e.square();
    let x = f - d.double();
    Point { x, y: (e * (d - x)) - c.double().double().double(), z: (self.y * self.z).double() }
  }
}

impl Sum<Point> for Point {
  fn sum<I: Iterator<Item = Point>>(iter: I) -> Point {
    let mut res = Self::identity();
    for i in iter {
      res += i;
    }
    res
  }
}

impl<'a> Sum<&'a Point> for Point {
  fn sum<I: Iterator<Item = &'a Point>>(iter: I) -> Point {
    Point::sum(iter.cloned())
  }
}

impl Mul<Scalar> for Point {
  type Output = Point;
  fn mul(self, other: Scalar) -> Point {
    // Precompute the optimal amount that's a multiple of 2
    let mut table = [Point::identity(); 16];
    table[1] = self;
    for i in 2 .. 16 {
      table[i] = table[i - 1] + self;
    }

    let mut res = Self::identity();
    let mut bits = 0;
    for (i, bit) in other.to_le_bits().iter().rev().enumerate() {
      bits <<= 1;
      let bit = *bit as u8;
      assert_eq!(bit | 1, 1);
      bits |= bit;

      if ((i + 1) % 4) == 0 {
        if i != 3 {
          for _ in 0 .. 4 {
            res = res.double();
          }
        }
        res += table[usize::from(bits)];
        bits = 0;
      }
    }
    res
  }
}

impl MulAssign<Scalar> for Point {
  fn mul_assign(&mut self, other: Scalar) {
    *self = *self * other;
  }
}

impl Mul<&Scalar> for Point {
  type Output = Point;
  fn mul(self, other: &Scalar) -> Point {
    self * *other
  }
}

impl MulAssign<&Scalar> for Point {
  fn mul_assign(&mut self, other: &Scalar) {
    *self *= *other;
  }
}

impl GroupEncoding for Point {
  type Repr = <FieldElement as PrimeField>::Repr;

  // TODO: Torsion clear, not check, or use a Ristretto-esque encoding
  fn from_bytes(bytes: &Self::Repr) -> CtOption<Self> {
    // Extract and clear the sign bit
    let sign = Choice::from(bytes[32] >> 7);
    let mut bytes = *bytes;
    let mut_ref: &mut [u8] = bytes.as_mut();
    mut_ref[32] &= !(1 << 7);

    // Parse x, recover y
    FieldElement::from_repr(bytes).and_then(|x| {
      recover_y(x).and_then(|mut y| {
        // Negate if the sign doesn't match
        y.conditional_negate(!y.is_odd().ct_eq(&sign));
        let infinity = x.ct_eq(&FieldElement::zero());
        let point = Point {
          x,
          y,
          z: FieldElement::conditional_select(
            &FieldElement::one(),
            &FieldElement::zero(),
            infinity,
          ),
        };
        let negative_infinity = infinity & sign;
        let torsioned = !(point * ORDER).ct_eq(&Point::identity());
        CtOption::new(point, !(negative_infinity | torsioned))
      })
    })
  }

  fn from_bytes_unchecked(bytes: &Self::Repr) -> CtOption<Self> {
    Point::from_bytes(bytes)
  }

  fn to_bytes(&self) -> Self::Repr {
    let z2 = self.z.square();
    let z3 = z2 * self.z;
    let x = self.x * z2.invert().unwrap_or(FieldElement::zero());
    let y = self.y * z3.invert().unwrap_or(FieldElement::zero());

    // Uses LE sign-bit encoding, traditional to Edwards, despite being short Weierstrass/Koblitz.
    // Does not use SEC1.
    let mut bytes = x.to_repr();
    let mut_ref: &mut [u8] = bytes.as_mut();
    mut_ref[32] |= y.is_odd().unwrap_u8() << 7;
    bytes
  }
}

impl PrimeGroup for Point {}

impl Point {
  pub fn mul_by_cofactor(&self) -> Point {
    // TODO: Use a re-addition formula
    let two = self.double();
    two.double() + two
  }
}

#[test]
fn serialize() {
  assert_eq!(Scalar::from_repr(Scalar::one().to_repr()).unwrap(), Scalar::one());
  assert_eq!(Point::from_bytes(&Point::generator().to_bytes()).unwrap(), Point::generator());
}

#[test]
fn eq() {
  assert_eq!(Point::identity(), Point::identity());
  assert_eq!(Point::generator(), Point::generator());
  assert!(Point::generator() != Point::identity());
}

#[test]
fn inverse() {
  assert_eq!(Scalar::one().invert().unwrap(), Scalar::one());
}

#[test]
fn add() {
  let two = Point::generator() + Point::generator();
  assert_eq!(Point::generator().double(), two);
  assert_eq!(two - Point::generator(), Point::generator());
  assert_eq!(Point::generator() - Point::generator(), Point::identity());
  assert_eq!([Point::generator(), Point::generator()].iter().sum::<Point>(), two);
}

#[test]
fn mul() {
  let two = Point::generator() + Point::generator();
  assert_eq!(Point::generator() * Scalar::from(2u8), two);
  assert_eq!(Point::generator() * Scalar::from(3u8), two + Point::generator());

  assert_eq!(
    (Point::generator() * Scalar::from(3u8)) + (Point::generator() * Scalar::from(2u8)),
    Point::generator() +
      Point::generator() +
      Point::generator() +
      Point::generator() +
      Point::generator()
  );

  assert_eq!((Point::generator() * -Scalar::one()) + Point::generator(), Point::identity());
  assert_eq!(Point::generator() * -Scalar::one(), -Point::generator());
  assert_eq!(
    (Point::generator() * -Scalar::one()) + Point::generator().double(),
    Point::generator()
  );
}

#[test]
fn infinity() {
  assert_eq!(Point::from_bytes(&Point::identity().to_bytes()).unwrap(), Point::identity());
  assert_eq!(Point::identity() + Point::identity(), Point::identity());
  assert_eq!(Point::identity() + Point::generator(), Point::generator());
  assert_eq!(Point::generator() + Point::identity(), Point::generator());
}

#[test]
fn field() {
  let zero = (Point::generator() * -Scalar::one()) + Point::generator();
  assert_eq!(zero.to_bytes(), <Point as GroupEncoding>::Repr::default());
  Point::from_bytes(&zero.to_bytes()).unwrap();
  assert_eq!(zero, Point::identity());
}

#[test]
fn torsion() {
  assert!(bool::from(
    !((Point { x: G_X, y: recover_y(G_X).unwrap(), z: FieldElement::one() }) * ORDER)
      .ct_eq(&Point::identity())
  ));
  assert!(bool::from((*G * ORDER).ct_eq(&Point::identity())));
}
