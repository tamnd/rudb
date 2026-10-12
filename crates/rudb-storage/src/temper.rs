//! When a frozen stripe is thawed and a thawed one frozen back, `10-freezing-and-checkpoint.md`
//! section 10.4.
//!
//! A frozen stripe whose rows keep being updated pays a migration for each update, and a hot one
//! pays far less, so a stripe is thawed when the time its migrations would cost over the next
//! minute pays for the thaw and for the refreeze that follows it. The inputs are measured: the
//! stripe's migration rate, the cost of a migration and of an update in place, and the time the
//! stripe's last thaw and freeze took. A thaw also has to fit under three quarters of the hot
//! set's limit, and comes no sooner than a minute after the stripe was frozen.
//!
//! A thawed stripe goes back once its in-place write rate has stayed under a quarter of its thaw
//! threshold for a minute, and no sooner than a minute after its thaw. The factor of four and the
//! minute each way are the hysteresis: a stripe whose rate sits near the threshold stays as it
//! is. A stripe thawed again within ten minutes of a freeze doubles its `k`, the factor on the
//! cost side, up to 8, and `k` halves for every ten minutes the stripe spends frozen.
//!
//! The rates are counters the stripes keep, [`crate::FrozenStripe::migrations`] and
//! [`crate::HotStripe::writes`], which [`Temper::look`] reads on each round and decays with a
//! 10 second half-life. The stripes pay one relaxed add a write for it.

use std::collections::HashMap;
use std::sync::Arc;

use crate::deletes::{PART_ROWS, PART_WORDS, STRIPE_ROWS};
use crate::stripes::{Stripe, StripeDirectory};

/// The half-life of a [`Rate`], in milliseconds.
const HALF_LIFE_MS: f64 = 10_000.0;

/// How far ahead a thaw has to pay for itself, H, in seconds.
const AHEAD_S: f64 = 60.0;

/// The least time a stripe stays frozen or thawed, and stays quiet before it is frozen back, in
/// milliseconds.
const STAY_MS: u64 = 60_000;

/// A thaw this soon after a freeze doubles `k`, and `k` halves for each of these spent frozen, in
/// milliseconds.
const BACKOFF_MS: u64 = 600_000;

/// The largest `k`.
const MOST_K: u32 = 8;

/// A count of events decayed with a 10 second half-life, which reads as their rate.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rate {
    value: f64,
    at_ms: u64,
}

impl Rate {
    /// Adds `events` at `now_ms`.
    pub fn add(&mut self, now_ms: u64, events: f64) {
        self.decay(now_ms);
        self.value += events;
    }

    /// Events a second as of `now_ms`. Under a steady rate the decayed count settles at the rate
    /// times the half-life over `ln 2`, so this is the count times `ln 2` over the half-life.
    #[must_use]
    pub fn per_second(&mut self, now_ms: u64) -> f64 {
        self.decay(now_ms);
        self.value * std::f64::consts::LN_2 / (HALF_LIFE_MS / 1_000.0)
    }

    fn decay(&mut self, now_ms: u64) {
        if now_ms > self.at_ms {
            self.value *= 0.5_f64.powf((now_ms - self.at_ms) as f64 / HALF_LIFE_MS);
            self.at_ms = now_ms;
        }
    }
}

/// The measured costs the rule weighs, kept as running averages.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Costs {
    /// Moving a frozen row into the head, `c_mig`, in nanoseconds.
    pub migrate_ns: f64,
    /// Updating a hot row in place, `c_hot`, in nanoseconds.
    pub hot_ns: f64,
}

impl Default for Costs {
    /// The section's defaults, until the head has measured both.
    fn default() -> Self {
        Self { migrate_ns: 6_000.0, hot_ns: 500.0 }
    }
}

impl Costs {
    /// Folds a migration that took `ns` into its average.
    pub fn migrated(&mut self, ns: u64) {
        self.migrate_ns = average(self.migrate_ns, ns);
    }

    /// Folds an update in place that took `ns` into its average.
    pub fn updated(&mut self, ns: u64) {
        self.hot_ns = average(self.hot_ns, ns);
    }
}

fn average(mean: f64, ns: u64) -> f64 {
    mean + (ns as f64 - mean) / 64.0
}

/// How often the stripes of one table changed form.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Turns {
    /// Stripes thawed.
    pub thaws: u64,
    /// Stripes frozen, by the freezer for any reason.
    pub freezes: u64,
    /// Thaws within ten minutes of the stripe's freeze, which the backoff counts against it.
    pub flips: u64,
}

/// What one stripe did, which outlives each form it takes.
#[derive(Debug, Clone)]
struct History {
    /// Whether it was hot at the last look, and its counter then.
    hot: bool,
    seen: u64,
    /// Its migration rate while frozen, its write rate while hot.
    rate: Rate,
    frozen_ms: Option<u64>,
    thawed_ms: Option<u64>,
    k: u32,
    thaw_ns: Option<f64>,
    refreeze_ns: Option<f64>,
    /// Since when its write rate has been under a quarter of its threshold, while hot.
    quiet_since: Option<u64>,
}

impl History {
    fn new(hot: bool, seen: u64) -> Self {
        Self {
            hot,
            seen,
            rate: Rate::default(),
            frozen_ms: None,
            thawed_ms: None,
            k: 1,
            thaw_ns: None,
            refreeze_ns: None,
            quiet_since: None,
        }
    }

    /// `k` as of `now_ms`, halved for every ten minutes since the freeze.
    fn k(&self, now_ms: u64) -> u32 {
        let halvings = self.frozen_ms.map_or(0, |at| now_ms.saturating_sub(at) / BACKOFF_MS);
        (self.k >> halvings.min(31)).max(1)
    }
}

/// The stripes the rule picked on one look, as `(table, stripe id)` with `table` an index into
/// the tables looked at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Picked {
    /// Frozen stripes to thaw, with [`StripeDirectory::begin_thaw`].
    pub thaw: Vec<(usize, u32)>,
    /// Thawed stripes to freeze back, with [`StripeDirectory::begin_freeze`].
    pub freeze: Vec<(usize, u32)>,
}

/// The thaw rule's state across the tables.
#[derive(Debug, Clone)]
pub struct Temper {
    costs: Costs,
    /// The hot set's limit in bytes, `hot_limit`.
    hot_limit: usize,
    stripes: HashMap<(u32, u32), History>,
    turns: HashMap<u32, Turns>,
}

impl Temper {
    /// The rule for a hot set limited to `hot_limit` bytes, with the default costs.
    #[must_use]
    pub fn new(hot_limit: usize) -> Self {
        Self { costs: Costs::default(), hot_limit, stripes: HashMap::new(), turns: HashMap::new() }
    }

    /// The costs, for the head to fold its measurements into.
    pub fn costs(&mut self) -> &mut Costs {
        &mut self.costs
    }

    /// The thaws, freezes and flips of table `table` so far.
    #[must_use]
    pub fn turns(&self, table: u32) -> Turns {
        self.turns.get(&table).copied().unwrap_or_default()
    }

    /// Reads every stripe's counter at `now_ms` and picks the frozen stripes worth thawing and the
    /// thawed ones quiet enough to freeze back. A stripe thawed here counts against the room in
    /// the hot set for the next one.
    pub fn look(&mut self, tables: &[Arc<StripeDirectory>], now_ms: u64) -> Picked {
        let mut hot_bytes: usize = tables.iter().map(|table| table.hot_bytes()).sum();
        let mut picked = Picked::default();
        for (index, table) in tables.iter().enumerate() {
            for stripe in &*table.stripes() {
                let (hot, count, rows) = match stripe {
                    Stripe::Frozen(frozen) if !frozen.is_thawing() => {
                        (false, frozen.migrations(), frozen.rows())
                    }
                    Stripe::Hot(stripe) if stripe.is_thawed() && !stripe.is_freezing() => {
                        (true, stripe.writes(), stripe.reserved())
                    }
                    _ => continue,
                };
                let history = self
                    .stripes
                    .entry((table.table(), stripe.id()))
                    .or_insert_with(|| History::new(hot, count));
                if history.hot != hot {
                    (history.hot, history.seen, history.rate) = (hot, 0, Rate::default());
                    history.quiet_since = None;
                }
                history.rate.add(now_ms, count.saturating_sub(history.seen) as f64);
                history.seen = count;
                let rate = history.rate.per_second(now_ms);
                let threshold = threshold(&self.costs, history, rows, now_ms);
                if hot {
                    if rate >= threshold / 4.0 {
                        history.quiet_since = None;
                        continue;
                    }
                    let quiet = *history.quiet_since.get_or_insert(now_ms);
                    let thawed = history.thawed_ms.unwrap_or(0);
                    if now_ms.saturating_sub(quiet) >= STAY_MS
                        && now_ms.saturating_sub(thawed) >= STAY_MS
                    {
                        picked.freeze.push((index, stripe.id()));
                    }
                } else {
                    let size = hot_size(table, rows);
                    let settled =
                        history.frozen_ms.is_none_or(|at| now_ms.saturating_sub(at) >= STAY_MS);
                    if rate >= threshold
                        && settled
                        && (hot_bytes + size) as f64 <= 0.75 * self.hot_limit as f64
                    {
                        hot_bytes += size;
                        picked.thaw.push((index, stripe.id()));
                    }
                }
            }
        }
        picked
    }

    /// Notes that stripe `id` of table `table` was thawed at `now_ms` in `ns`. A thaw within ten
    /// minutes of its freeze doubles its `k`, up to 8.
    pub fn thawed(&mut self, table: u32, id: u32, now_ms: u64, ns: u64) {
        let history = self.stripes.entry((table, id)).or_insert_with(|| History::new(true, 0));
        let turns = self.turns.entry(table).or_default();
        turns.thaws += 1;
        history.k = history.k(now_ms);
        if history.frozen_ms.is_some_and(|at| now_ms.saturating_sub(at) < BACKOFF_MS) {
            history.k = (history.k * 2).min(MOST_K);
            turns.flips += 1;
        }
        history.thawed_ms = Some(now_ms);
        history.thaw_ns = Some(ns as f64);
        (history.hot, history.seen, history.rate) = (true, 0, Rate::default());
        history.quiet_since = None;
    }

    /// Notes that stripe `id` of table `table` was frozen at `now_ms`, its encode taking `ns`,
    /// which is what its next refreeze is expected to cost.
    pub fn froze(&mut self, table: u32, id: u32, now_ms: u64, ns: u64) {
        let history = self.stripes.entry((table, id)).or_insert_with(|| History::new(false, 0));
        self.turns.entry(table).or_default().freezes += 1;
        history.k = history.k(now_ms);
        history.frozen_ms = Some(now_ms);
        history.refreeze_ns = Some(ns as f64);
        (history.hot, history.seen, history.rate) = (false, 0, Rate::default());
        history.quiet_since = None;
    }
}

/// The migration rate, events a second, above which a frozen stripe of `rows` rows is worth
/// thawing: `r * (c_mig - c_hot) * H >= k * (c_thaw + c_refreeze)`. Before the freezer measured
/// the stripe, a thaw is taken to cost 50 ms and a freeze 2 s for a full stripe, and in proportion
/// for a shorter one, which lets a table of a few rows thaw on its first few hundred updates.
fn threshold(costs: &Costs, history: &History, rows: u32, now_ms: u64) -> f64 {
    let share = f64::from(rows) / f64::from(STRIPE_ROWS);
    let thaw = history.thaw_ns.unwrap_or(50e6 * share);
    let refreeze = history.refreeze_ns.unwrap_or(2e9 * share);
    let saved = costs.migrate_ns - costs.hot_ns;
    if saved <= 0.0 {
        return f64::INFINITY;
    }
    f64::from(history.k(now_ms)) * (thaw + refreeze) / (saved * AHEAD_S)
}

/// Bytes a thaw of `rows` rows of `table` adds to the hot set, `M_s`: every part's columns and
/// validity bits and its stamps, as [`crate::HotStripe::bytes`] counts them, without the strings
/// a text column puts in its arena.
fn hot_size(table: &StripeDirectory, rows: u32) -> usize {
    let parts = rows.div_ceil(PART_ROWS) as usize;
    let rows = PART_ROWS as usize;
    let columns: usize =
        table.widths().iter().map(|width| width.bytes() * rows + PART_WORDS * 8).sum();
    parts * (columns + rows * (3 * 8 + 4))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Rate, Temper, Turns};
    use crate::deletes::{DeleteVector, STRIPE_ROWS};
    use crate::hot::Width;
    use crate::stripes::{Stripe, StripeDirectory};
    use crate::undo::UndoSpace;

    #[test]
    fn a_rate_settles_at_the_rate_and_halves_in_ten_seconds() {
        let mut rate = Rate::default();
        for tick in 1..=600 {
            rate.add(tick * 100, 1_000.0);
        }
        let settled = rate.per_second(60_000);
        assert!((settled - 10_000.0).abs() < 200.0, "{settled} a second");
        let later = rate.per_second(70_000);
        assert!((later - settled / 2.0).abs() < 1.0, "{later} ten seconds on");
    }

    fn table(id: u32) -> Arc<StripeDirectory> {
        let undos = Arc::new(UndoSpace::new());
        Arc::new(StripeDirectory::new(&[Width::Eight], undos).in_table(id))
    }

    /// Runs `temper` every 100 ms from `from` to `to` milliseconds with stripe `id` of `table`
    /// moving `per_second` rows out a second, and returns what each look picked to thaw.
    fn migrate(
        temper: &mut Temper,
        table: &Arc<StripeDirectory>,
        id: u32,
        per_second: u64,
        (from, to): (u64, u64),
    ) -> Vec<(usize, u32)> {
        let mut thaw = Vec::new();
        for now in (from..to).step_by(100) {
            if let Some(Stripe::Frozen(frozen)) = table.find(id) {
                frozen.add_migrations(per_second / 10);
            }
            thaw.extend(temper.look(&[Arc::clone(table)], now).thaw);
        }
        thaw
    }

    #[test]
    fn a_full_stripe_thaws_past_about_6200_migrations_a_second() {
        // 2.05 s of thaw and refreeze over 5.5 us saved a migration for 60 s.
        let items = table(1);
        let id = items.attach(STRIPE_ROWS, 1, DeleteVector::new());
        let mut temper = Temper::new(1 << 40);
        assert!(migrate(&mut temper, &items, id, 5_000, (0, 60_000)).is_empty());
        let thaw = migrate(&mut temper, &items, id, 7_000, (60_000, 120_000));
        assert_eq!(thaw.first(), Some(&(0, id)));

        // A table of a few rows thaws on a few hundred updates a minute.
        let small = table(2);
        let id = small.attach(10, 1, DeleteVector::new());
        let mut temper = Temper::new(1 << 40);
        assert_eq!(migrate(&mut temper, &small, id, 10, (0, 60_000)).first(), Some(&(0, id)));
    }

    #[test]
    fn a_thaw_waits_a_minute_after_a_freeze_and_for_room_in_the_hot_set() {
        let items = table(1);
        let id = items.attach(STRIPE_ROWS, 1, DeleteVector::new());
        let mut temper = Temper::new(1 << 40);
        temper.froze(1, id, 0, 2_000_000_000);
        assert!(migrate(&mut temper, &items, id, 20_000, (0, 60_000)).is_empty());
        assert!(!migrate(&mut temper, &items, id, 20_000, (60_000, 61_000)).is_empty());
        let fresh = migrate(&mut Temper::new(1 << 40), &items, id, 20_000, (0, 60_000));
        assert!(!fresh.is_empty(), "with no freeze to wait out it thaws within the minute");

        // A full stripe of one 8-byte column is about 18 MiB hot, over three quarters of 16 MiB.
        let mut tight = Temper::new(16 << 20);
        assert!(migrate(&mut tight, &items, id, 20_000, (0, 60_000)).is_empty());
        let mut roomy = Temper::new(32 << 20);
        assert!(!migrate(&mut roomy, &items, id, 20_000, (0, 60_000)).is_empty());
    }

    #[test]
    fn a_thaw_soon_after_a_freeze_doubles_k_and_time_frozen_halves_it() {
        let items = table(1);
        let id = items.attach(STRIPE_ROWS, 1, DeleteVector::new());
        let mut temper = Temper::new(1 << 40);
        temper.froze(1, id, 0, 2_000_000_000);
        temper.thawed(1, id, 70_000, 50_000_000);
        assert_eq!(temper.turns(1), Turns { thaws: 1, freezes: 1, flips: 1 });
        temper.froze(1, id, 200_000, 2_000_000_000);
        // With k at 2 the threshold is about 12,400 a second.
        let thaw = migrate(&mut temper, &items, id, 10_000, (200_000, 320_000));
        assert!(thaw.is_empty(), "k is 2");
        let thaw = migrate(&mut temper, &items, id, 14_000, (320_000, 440_000));
        assert!(!thaw.is_empty(), "past twice the threshold");

        // Ten minutes frozen halves it back to 1.
        let mut temper = Temper::new(1 << 40);
        temper.froze(1, id, 0, 2_000_000_000);
        temper.thawed(1, id, 70_000, 50_000_000);
        temper.froze(1, id, 200_000, 2_000_000_000);
        let thaw = migrate(&mut temper, &items, id, 9_000, (800_000, 920_000));
        assert!(!thaw.is_empty(), "k halved after ten minutes frozen");
    }

    #[test]
    fn a_thawed_stripe_freezes_back_after_a_quiet_minute() {
        let items = table(1);
        let id = items.attach(STRIPE_ROWS, 1, DeleteVector::new());
        let frozen = items.begin_thaw(id).expect("a frozen stripe");
        let hot = items.thaw(&frozen, |_| {});
        let mut temper = Temper::new(1 << 40);
        temper.froze(1, id, 0, 2_000_000_000);
        temper.thawed(1, id, 700_000, 50_000_000);
        assert_eq!(temper.turns(1), Turns { thaws: 1, freezes: 1, flips: 0 });
        // The thaw threshold is about 6,200 a second, a quarter of it about 1,550. The rate
        // starts at 0 and passes that 22 s into the first stretch, before a minute is up.
        let mut freeze = Vec::new();
        for now in (700_000..820_000).step_by(100) {
            hot.add_writes(200);
            freeze.extend(temper.look(&[Arc::clone(&items)], now).freeze);
        }
        assert!(freeze.is_empty(), "2,000 writes a second is not quiet");
        // At 1,000 a second the rate is under the quarter 9 s in.
        for now in (820_000..880_000).step_by(100) {
            hot.add_writes(100);
            freeze.extend(temper.look(&[Arc::clone(&items)], now).freeze);
        }
        assert!(freeze.is_empty(), "1,000 a second has to last a minute first");
        for now in (880_000..920_000).step_by(100) {
            hot.add_writes(100);
            freeze.extend(temper.look(&[Arc::clone(&items)], now).freeze);
        }
        assert_eq!(freeze.first(), Some(&(0, id)));
    }
}
