//! `random()`, `setseed()` and the UUID makers, drawn the way the pin draws them so a seeded query
//! repeats its rows.
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

use rudb_common::{Error, LogicalType, Result, Value, uuid};
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

/// Whether `name` is a call with no arguments that the executor has to give a row count to.
#[must_use]
pub fn draws(name: &str) -> bool {
    matches!(name, "random" | "gen_random_uuid" | "uuid" | "uuidv4" | "uuidv7")
}

/// `rows` answers for one call to `name`, which [`draws`] said yes to.
///
/// # Errors
///
/// When `name` is not one of them.
pub fn drawn(name: &str, rows: usize) -> Result<Vector> {
    match name {
        "random" => random(rows),
        "gen_random_uuid" | "uuid" | "uuidv4" => uuids(rows, version4),
        "uuidv7" => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
            let millis = now.map_or(0, |now| now.as_millis());
            uuids(rows, |own| version7(own, millis))
        }
        _ => Err(Error::internal(format!("{name} is not drawn"))),
    }
}

/// `rows` UUIDs, each made by `make` from a generator seeded the way `random()` seeds its own.
fn uuids(rows: usize, mut make: impl FnMut(&mut Pcg32) -> [u8; 16]) -> Result<Vector> {
    let mut own = Pcg32::seeded(with_shared(Pcg32::next64));
    let values: Vec<i128> = (0..rows).map(|_| uuid::from_bytes(make(&mut own))).collect();
    Vector::flat(LogicalType::Uuid, Data::Int128(values.into()))
}

/// A version 4 UUID the pin's way: four outputs laid down low byte first, the version and variant
/// bits put in, and the top bit flipped, since the pin builds the number without the flip its
/// stored form has and so every one it prints has that bit the other way round.
fn version4(own: &mut Pcg32) -> [u8; 16] {
    let mut bytes = [0; 16];
    for chunk in bytes.chunks_exact_mut(4) {
        chunk.copy_from_slice(&own.next().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes[0] ^= 0x80;
    bytes
}

/// A version 7 UUID the pin's way: the forty eight bits of `millis`, then ten random bytes from
/// three outputs laid down high byte first, with the version and variant bits put in.
fn version7(own: &mut Pcg32, millis: u128) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[10..]);
    let mut random = [0; 12];
    for chunk in random.chunks_exact_mut(4) {
        chunk.copy_from_slice(&own.next().to_be_bytes());
    }
    bytes[6..].copy_from_slice(&random[..10]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes
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

    #[test]
    fn a_seeded_generator_makes_the_pins_uuids() {
        // `setseed(0.5)` on the pin, then `SELECT uuid(), uuidv7() FROM range(2)`, whose second
        // call takes the second seed from the shared generator.
        let mut shared = Pcg32::seeded(u64::from(((0.5 + 1.0) * f64::from(u32::MAX / 2)) as u32));
        let text = |bytes: [u8; 16]| Value::Uuid(uuid::from_bytes(bytes)).to_string();
        let mut own = Pcg32::seeded(shared.next64());
        assert_eq!(text(version4(&mut own)), "4e8de2d9-2ca9-4c5a-8baa-9a9005b24344");
        assert_eq!(text(version4(&mut own)), "f8db6a10-3fb6-41db-b068-b3ba73ab29fd");
        let mut own = Pcg32::seeded(shared.next64());
        assert_eq!(text(version4(&mut own)), "8b31c300-db9f-45ea-b7ce-319e8056e58c");
        let mut shared = Pcg32::seeded(u64::from(((0.5 + 1.0) * f64::from(u32::MAX / 2)) as u32));
        let mut own = Pcg32::seeded(shared.next64());
        let millis = 0x01a0_edf1_eca9;
        assert_eq!(text(version7(&mut own, millis)), "01a0edf1-eca9-79e2-8dce-5acca92c909a");
        assert_eq!(text(version7(&mut own, millis)), "01a0edf1-eca9-7443-b205-106adb78dbd1");
    }
}
