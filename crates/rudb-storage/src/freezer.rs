//! The freezer's worker, `10-freezing-and-checkpoint.md` sections 10.2 to 10.4.
//!
//! One background worker runs a [`Freezer`] a pass at a time. A pass begins the freezes of the
//! stripes [`to_freeze`] picks, the quiet sealed ones and, while the hot set is over `hot_limit`,
//! the coldest, along with the thawed stripes the thaw rule says are quiet again. It thaws the
//! frozen stripes the rule says are worth it. Then it moves every freeze it holds on as far as it
//! can go: a stripe whose rows are final is encoded, one stripe at a time, and swapped in. A freeze
//! that has to wait for a write in flight is kept for the next pass, so a pass never blocks on a
//! writer. Nor on a reader: a statement with a snapshot from before a stripe's F reads what it
//! sees differently out of the stripe's graveyard, see [`crate::graveyard`], and each pass drops
//! the graveyards no statement needs any more.
//!
//! The pages are the file's business, so the freezer is handed an [`Encoder`] that writes a
//! stripe's pages and reads them back for a thaw. Nothing it writes is referenced until the next
//! checkpoint round publishes the stripe, see [`crate::round`].

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

use rudb_common::Result;

use crate::horizon::Horizons;
use crate::hot::HotStripe;
use crate::stripes::{Freeze, FrozenStripe, Stripe, StripeDirectory, to_freeze};
use crate::temper::Temper;

/// How long a sealed stripe goes without a commit before it is frozen, `freeze_quiet_s`.
pub const QUIET_MS: u64 = 30_000;

/// The share of `memory_limit` the hot set may hold, `15-resources.md`.
#[must_use]
pub fn hot_limit(memory_limit: usize) -> usize {
    memory_limit / 2
}

/// What the freezer needs of the file.
pub trait Encoder {
    /// What encoding a stripe leaves for the round that publishes it: its pages and root, written
    /// and not yet referenced.
    type Written;
    /// What reading a frozen stripe's pages gives a thaw.
    type Read;

    /// Encodes `freeze`'s stripe of `table` as of its F and writes it.
    ///
    /// # Errors
    ///
    /// If the pages cannot be written. The freeze is kept and tried again on the next pass.
    fn encode(&mut self, table: &StripeDirectory, freeze: &Freeze) -> Result<Self::Written>;

    /// Notes that `frozen`, the stripe `written` holds, took the hot stripe's place in `table`.
    fn swapped(
        &mut self,
        table: &StripeDirectory,
        frozen: &Arc<FrozenStripe>,
        written: Self::Written,
    );

    /// Reads the pages of `frozen`, a stripe of `table`, as of its checkpoint.
    ///
    /// # Errors
    ///
    /// If the pages cannot be read. The stripe stays frozen.
    fn read(&mut self, table: &StripeDirectory, frozen: &FrozenStripe) -> Result<Self::Read>;

    /// Writes every slot's values `read` holds into `hot`, the stripe a thaw is making.
    fn fill(&mut self, read: Self::Read, hot: &HotStripe);
}

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pass {
    /// Freezes begun.
    pub began: usize,
    /// Stripes encoded.
    pub encoded: usize,
    /// Frozen stripes swapped in.
    pub swapped: usize,
    /// Stripes thawed.
    pub thawed: usize,
    /// Freezes still held, waiting for a write in flight or for an encode that failed.
    pub waiting: usize,
    /// Graveyards dropped, see [`StripeDirectory::bury`].
    pub buried: usize,
}

/// A freeze the freezer holds, by the table it is of.
#[derive(Debug)]
enum Held {
    /// Begun, its rows not yet final.
    Settling(Arc<StripeDirectory>, Arc<HotStripe>),
    /// Its rows final, not yet encoded, which is where a failed encode leaves it.
    Settled(Arc<StripeDirectory>, Freeze),
}

/// The freezer's state across passes.
#[derive(Debug)]
pub struct Freezer {
    hot_limit: usize,
    quiet_ms: u64,
    temper: Temper,
    held: Vec<Held>,
}

impl Freezer {
    /// A freezer for a hot set of at most `hot_limit` bytes, which freezes a sealed stripe once it
    /// has gone `quiet_ms` without a commit.
    #[must_use]
    pub fn new(hot_limit: usize, quiet_ms: u64) -> Self {
        Self { hot_limit, quiet_ms, temper: Temper::new(hot_limit), held: Vec::new() }
    }

    /// The thaw rule, for the head to fold its costs into and `rudb_stats` to read the turns of.
    pub fn temper(&mut self) -> &mut Temper {
        &mut self.temper
    }

    /// Freezes held, begun and not yet swapped in.
    #[must_use]
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// One pass over `tables` at `now_ms`, see the module documentation.
    ///
    /// # Errors
    ///
    /// The first error of `encoder`. The pass stops there, and what it had not done is done by the
    /// next one.
    pub fn pass<E: Encoder>(
        &mut self,
        tables: &[Arc<StripeDirectory>],
        horizons: &Horizons,
        clock: &AtomicU64,
        now_ms: u64,
        encoder: &mut E,
    ) -> Result<Pass> {
        let mut pass = Pass::default();
        for table in tables {
            pass.buried += table.bury(horizons, clock);
        }
        let picked = self.temper.look(tables, now_ms);
        let cold = to_freeze(tables, now_ms, self.quiet_ms, self.hot_limit);
        for (index, id) in cold.into_iter().chain(picked.freeze) {
            let table = &tables[index];
            if let Some(hot) = table.begin_freeze(id) {
                self.held.push(Held::Settling(Arc::clone(table), hot));
                pass.began += 1;
            }
        }
        for (index, id) in picked.thaw {
            let table = &tables[index];
            let Some(Stripe::Frozen(frozen)) = table.find(id) else { continue };
            let started = Instant::now();
            let read = encoder.read(table, &frozen)?;
            let Some(frozen) = table.begin_thaw(id) else { continue };
            table.thaw(&frozen, |hot| encoder.fill(read, hot));
            self.temper.thawed(table.table(), id, now_ms, elapsed_ns(started));
            pass.thawed += 1;
        }
        let mut held = Vec::with_capacity(self.held.len());
        let mut failed = None;
        for mut one in std::mem::take(&mut self.held) {
            if failed.is_none()
                && let Held::Settling(table, hot) = &one
                && let Some(freeze) = table.settle(hot, clock)
            {
                one = Held::Settled(Arc::clone(table), freeze);
            }
            if failed.is_none()
                && let Held::Settled(table, freeze) = &one
            {
                let started = Instant::now();
                match encoder.encode(table, freeze) {
                    Ok(written) => {
                        let ns = elapsed_ns(started);
                        pass.encoded += 1;
                        let Held::Settled(table, freeze) = one else { unreachable!() };
                        let id = freeze.stripe().id();
                        let frozen = table.publish(freeze, horizons);
                        encoder.swapped(&table, &frozen, written);
                        self.temper.froze(table.table(), id, now_ms, ns);
                        pass.swapped += 1;
                        continue;
                    }
                    Err(error) => failed = Some(error),
                }
            }
            held.push(one);
        }
        self.held = held;
        pass.waiting = self.held.len();
        match failed {
            Some(error) => Err(error),
            None => Ok(pass),
        }
    }
}

fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use rudb_common::{Error, Result};

    use super::{Encoder, Freezer, Pass};
    use crate::deletes::{Base, DeleteVector};
    use crate::graveyard::Cell;
    use crate::horizon::{Horizons, Reads};
    use crate::hot::{HotStripe, Width};
    use crate::park::Wait;
    use crate::stripes::{Freeze, FrozenStripe, Inserter, Stripe, StripeDirectory};
    use crate::undo::{UndoBuffer, UndoSpace};

    /// Keeps each frozen stripe's values as of its F, the way pages would.
    #[derive(Default)]
    struct Pages {
        stripes: Vec<(u32, Vec<Option<u128>>)>,
        fail: bool,
        swapped: Vec<u32>,
    }

    impl Encoder for Pages {
        type Written = (u32, Vec<Option<u128>>);
        type Read = Vec<Option<u128>>;

        fn encode(&mut self, _: &StripeDirectory, freeze: &Freeze) -> Result<Self::Written> {
            if self.fail {
                return Err(Error::io("the disk is full"));
            }
            let hot = freeze.stripe();
            let values = (0..freeze.rows()).map(|slot| hot.read(slot, 0)).collect();
            Ok((hot.id(), values))
        }

        fn swapped(
            &mut self,
            _: &StripeDirectory,
            frozen: &Arc<FrozenStripe>,
            written: Self::Written,
        ) {
            assert_eq!(frozen.id(), written.0);
            self.swapped.push(written.0);
            self.stripes.push(written);
        }

        fn read(&mut self, _: &StripeDirectory, frozen: &FrozenStripe) -> Result<Self::Read> {
            let held = self.stripes.iter().find(|(id, _)| *id == frozen.id());
            Ok(held.map_or_else(|| vec![Some(1); frozen.rows() as usize], |(_, v)| v.clone()))
        }

        fn fill(&mut self, read: Self::Read, hot: &HotStripe) {
            for (slot, value) in read.into_iter().enumerate() {
                hot.write(slot as u32, 0, value);
            }
        }
    }

    fn directory() -> Arc<StripeDirectory> {
        Arc::new(StripeDirectory::new(&[Width::Eight], Arc::new(UndoSpace::new())))
    }

    /// `n` rows committed at `ts`, each holding its slot, the stripe sealed when `sealed`.
    fn stripe(table: &Arc<StripeDirectory>, n: u32, ts: u64, sealed: bool) -> Arc<HotStripe> {
        let mut inserter = Inserter::new(Arc::clone(table));
        let (lease, slots) = inserter.take(n);
        for slot in slots.clone() {
            lease.stripe().write(slot, 0, Some(u128::from(slot)));
        }
        assert!(lease.stripe().insert(slots.clone(), 1));
        lease.stripe().commit_insert(slots, 1, ts);
        let hot = Arc::clone(lease.stripe());
        if sealed {
            assert!(hot.seal());
        }
        hot
    }

    #[test]
    fn a_quiet_sealed_stripe_is_encoded_and_swapped_in_and_an_open_one_left() {
        let table = directory();
        let (clock, horizons) = (AtomicU64::new(2), Horizons::new(1));
        let sealed = stripe(&table, 100, 2, true);
        let open = stripe(&table, 50, 2, false);
        sealed.touch(1_000);
        open.touch(1_000);
        let mut freezer = Freezer::new(usize::MAX, 30_000);
        let mut pages = Pages::default();
        let tables = [Arc::clone(&table)];
        let pass = freezer.pass(&tables, &horizons, &clock, 2_000, &mut pages).expect("pass");
        assert_eq!(pass, Pass::default(), "nothing quiet yet");

        // A snapshot from before F does not hold the swap back: the row written since goes in
        // the graveyard, which a pass drops once the statement ended.
        let old = horizons.begin(0, &clock, Reads::Tables(vec![0]));
        let mut buffer = UndoBuffer::default();
        sealed.update(7, &[(0, Some(70))], 2, 5, Wait::NEVER, &mut buffer).expect("update");
        sealed.commit_write(7, 5, 3);
        clock.store(3, Ordering::SeqCst);
        // The commit noted its time by the process's clock, which other tests have run on.
        sealed.touch(1_000);
        let pass = freezer.pass(&tables, &horizons, &clock, 31_000, &mut pages).expect("pass");
        assert_eq!((pass.began, pass.encoded, pass.swapped, pass.waiting), (1, 1, 1, 0));
        let Some(Stripe::Frozen(frozen)) = table.find(sealed.id()) else { panic!("frozen") };
        let graveyard = frozen.graveyard().expect("a snapshot from before");
        assert_eq!(graveyard.seen(7, old), Some(Some(&[Some(Cell::Fixed(7))][..])));
        assert!(matches!(table.find(open.id()), Some(Stripe::Hot(_))), "open, so left hot");
        assert_eq!(pages.swapped, [sealed.id()]);
        assert_eq!(pages.stripes[0].1[7], Some(70));
        assert_eq!((table.count(old, 0), table.count(3, 0)), (150, 150));
        assert_eq!(freezer.temper().turns(0).freezes, 1);
        let pass = freezer.pass(&tables, &horizons, &clock, 31_000, &mut pages).expect("pass");
        assert_eq!(pass, Pass::default(), "the statement still runs");
        horizons.end(0);
        let pass = freezer.pass(&tables, &horizons, &clock, 31_000, &mut pages).expect("pass");
        assert_eq!(pass, Pass { buried: 1, ..Pass::default() });
        assert!(frozen.graveyard().is_none());
    }

    #[test]
    fn over_the_limit_the_open_stripe_is_sealed_and_frozen_short() {
        let table = directory();
        let (clock, horizons) = (AtomicU64::new(2), Horizons::new(1));
        let open = stripe(&table, 10, 2, false);
        let mut freezer = Freezer::new(1, 30_000);
        let mut pages = Pages::default();
        let pass =
            freezer.pass(&[Arc::clone(&table)], &horizons, &clock, 0, &mut pages).expect("pass");
        assert_eq!((pass.began, pass.swapped), (1, 1));
        let Some(Stripe::Frozen(frozen)) = table.find(open.id()) else { panic!("frozen") };
        assert_eq!(frozen.rows(), open.reserved());
        let next = stripe(&table, 5, 3, false);
        assert_ne!(next.id(), open.id(), "inserts go to a new stripe");
    }

    #[test]
    fn a_failed_encode_keeps_the_freeze_for_the_next_pass() {
        let table = directory();
        let (clock, horizons) = (AtomicU64::new(2), Horizons::new(1));
        let hot = stripe(&table, 10, 2, true);
        hot.touch(0);
        let mut freezer = Freezer::new(usize::MAX, 0);
        let mut pages = Pages { fail: true, ..Pages::default() };
        let tables = [Arc::clone(&table)];
        assert!(freezer.pass(&tables, &horizons, &clock, 10, &mut pages).is_err());
        assert_eq!(freezer.held(), 1);
        assert!(matches!(table.find(hot.id()), Some(Stripe::Hot(_))));
        pages.fail = false;
        let pass = freezer.pass(&tables, &horizons, &clock, 10, &mut pages).expect("pass");
        assert_eq!((pass.began, pass.encoded, pass.swapped), (0, 1, 1));
        assert_eq!(freezer.held(), 0);
    }

    #[test]
    fn a_stripe_migrated_out_of_often_is_thawed_from_its_pages() {
        let table = directory();
        let (clock, horizons) = (AtomicU64::new(5), Horizons::new(1));
        let base: Vec<u32> = (0..10).collect();
        let id = table.attach(100, 3, DeleteVector::from_base(Base::of(&base)));
        let mut freezer = Freezer::new(usize::MAX, 30_000);
        let mut pages = Pages::default();
        let tables = [Arc::clone(&table)];
        freezer.pass(&tables, &horizons, &clock, 0, &mut pages).expect("a first look");
        let Some(Stripe::Frozen(frozen)) = table.find(id) else { panic!("frozen") };
        frozen.add_migrations(1_000_000);
        let pass = freezer.pass(&tables, &horizons, &clock, 1_000, &mut pages).expect("pass");
        assert_eq!(pass.thawed, 1);
        let Some(Stripe::Hot(hot)) = table.find(id) else { panic!("thawed") };
        assert_eq!(hot.read(50, 0), Some(1));
        assert_eq!(table.count(5, 0), 90, "the deletes came across");
        assert_eq!(freezer.temper().turns(0).thaws, 1);
    }
}
