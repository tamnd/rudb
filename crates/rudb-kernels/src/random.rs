//! `random()` and `setseed()`, drawn the way the pin draws them so a seeded query repeats its rows.
//!
//! The pin keeps one PCG32 generator per connection, which `setseed` reseeds, and gives every
//! `random()` in a query a generator of its own, seeded from sixty four bits of the shared one when
//! the query starts. A value is two 32 bit outputs put together and scaled into `[0, 1)`. Doing the
//! same here is what makes `SELECT setseed(0.5); SELECT random()` answer 0.8511131886287325 on both
//! engines.
//!
//! Two things differ. The shared generator belongs to the process rather than to a connection, so
//! two connections calling `setseed` move each other's sequence. And a call takes its seed once per
//! chunk rather than once per query, which gives the same numbers for any query of up to one chunk
//! and different, equally random, ones after that. Past one chunk the pin's own numbers depend on
//! how its threads split the rows, so nothing can repeat them there anyway.

use std::sync::{Mutex, PoisonError};

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::{Data, Vector};

/// The PCG32 the pin uses, `setseq_xsh_rr_64_32` on its default stream.
struct Pcg32 {
    state: u64,
}

const MULTIPLIER: u64 = 6_364_136_223_846_793_005;
const INCREMENT: u64 = 1_442_695_040_888_963_407;

impl Pcg32 {
    fn seeded(seed: u64) -> Self {
        Self { state: Self::bump(seed.wrapping_add(INCREMENT)) }
    }

    fn bump(state: u64) -> u64 {
        state.wrapping_mul(MULTIPLIER).wrapping_add(INCREMENT)
    }

    #[expect(clippy::cast_possible_truncation, reason = "the output is the low 32 bits by design")]
    fn next(&mut self) -> u32 {
        let old = self.state;
        self.state = Self::bump(old);
        let shifted = (((old >> 18) ^ old) >> 27) as u32;
        shifted.rotate_right((old >> 59) as u32)
    }

    fn next64(&mut self) -> u64 {
        (u64::from(self.next()) << 32) | u64::from(self.next())
    }

    /// A double in `[0, 1)` from sixty four bits, which is `std::ldexp(bits, -64)` on the pin.
    #[expect(clippy::cast_precision_loss, reason = "the rounding is the pin's too")]
    fn double(&mut self) -> f64 {
        self.next64() as f64 * (-64f64).exp2()
    }
}

/// The generator `setseed` moves, seeded from the clock and the process until something seeds it.
static SHARED: Mutex<Option<Pcg32>> = Mutex::new(None);

fn with_shared<T>(body: impl FnOnce(&mut Pcg32) -> T) -> T {
    let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
    body(shared.get_or_insert_with(|| Pcg32::seeded(entropy())))
}

/// Sixty four bits no two processes are likely to share, without a dependency to get them.
fn entropy() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        hasher.write_u128(now.as_nanos());
    }
    hasher.finish()
}

/// `rows` answers for one call to `random()`.
///
/// # Errors
///
/// None in practice. It is the error [`Vector::flat`] would report for a double vector of doubles.
pub fn random(rows: usize) -> Result<Vector> {
    let mut own = Pcg32::seeded(with_shared(Pcg32::next64));
    let values: Vec<f64> = (0..rows).map(|_| own.double()).collect();
    Vector::flat(LogicalType::Double, Data::Float64(values.into()))
}

/// `setseed(x)`, which reseeds the shared generator and answers NULL. A null seed leaves it alone.
pub(crate) fn setseed(seed: &Value) -> Result<Value> {
    let Value::Double(seed) = *seed else {
        return Ok(Value::Null);
    };
    if !(-1.0..=1.0).contains(&seed) {
        return Err(Error::invalid_input(
            "SETSEED accepts seed values between -1.0 and 1.0, inclusive",
        ));
    }
    let half = f64::from(u32::MAX / 2);
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "0 to u32::MAX")]
    let seed = ((seed + 1.0) * half) as u32;
    with_shared(|shared| *shared = Pcg32::seeded(u64::from(seed)));
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seeded_generator_repeats_the_pins_numbers() {
        // `setseed(0.5)` on the pin, then the first two values of the first `random()`.
        let mut shared = Pcg32::seeded(u64::from(((0.5 + 1.0) * f64::from(u32::MAX / 2)) as u32));
        let mut own = Pcg32::seeded(shared.next64());
        assert_eq!(own.double(), 0.851_113_188_628_732_5);
        assert_eq!(own.double(), 0.564_860_018_730_782_4);
        let mut second = Pcg32::seeded(shared.next64());
        assert_eq!(second.double(), 0.002_978_387_269_385_594);
    }
}
