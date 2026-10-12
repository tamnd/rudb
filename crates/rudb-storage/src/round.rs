//! Which stripes a checkpoint round writes, and how far the log can be cut behind it,
//! `engine-v4/10-freezing-and-checkpoint.md` sections 10.5 and 10.7.
//!
//! Every stripe has a checkpoint timestamp C_s, the cut its image in the file is the state as of,
//! and a debt, the log bytes charged to it by commits since then, which is what an open would
//! replay for it. A round at cut S does not write every stripe. It writes the ones with the most
//! debt until what is left is under half of `wal_replay_budget`, and the one with the oldest C_s
//! so the log base always moves. A frozen stripe whose delete list grew past 4,096 entries has
//! its delete vector folded and written, and a freeze that finished at or before S is published.
//! A stripe nothing changed since its image is bumped: its image is its state at S too, so its
//! C_s becomes S for the price of its directory entry, which is what lets the log base move past
//! the stripes of a large database that nobody writes.
//!
//! The log base is then the smallest C_s, with the catalog's and the key runs' own checkpoint
//! timestamps among them, and a lane's segment whose every commit is at or below it is recycled.

/// What a stripe is, as far as a round is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// A hot stripe, written as a hot image.
    Hot,
    /// A frozen stripe, whose delete list holds `listed` entries the file does not have yet.
    Frozen {
        /// Entries in the delete list.
        listed: usize,
    },
    /// A hot stripe the freezer encoded as of `at` and that waits to be published.
    Finished {
        /// The cut the freeze took, F.
        at: u64,
    },
}

/// A stripe as a round finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Standing {
    /// The table it belongs to.
    pub table: u32,
    /// Its id in the table.
    pub stripe: u32,
    /// C_s, the cut of its image in the file, 0 for a stripe with none yet.
    pub checkpoint: u64,
    /// The commit timestamp of its newest change.
    pub changed: u64,
    /// Log bytes charged to it since C_s.
    pub debt: u64,
    /// What it is.
    pub form: Form,
}

/// What a round does with a stripe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Write its hot image as of the cut, of what changed since C_s.
    Write,
    /// Fold the delete list of a frozen stripe into its base and write that.
    Fold,
    /// Publish the frozen stripe a freeze finished.
    Publish,
    /// Nothing changed since its image: only its C_s moves to the cut.
    Bump,
    /// Leave it for a later round.
    Keep,
}

/// The plan of one round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Round {
    /// The cut S the round writes as of.
    pub cut: u64,
    /// Each stripe's step and its C_s once the round is published, in the order given.
    pub steps: Vec<(Step, u64)>,
    /// The log base once it is published: no record at or below it is needed again.
    pub base: u64,
    /// The debt the stripes it keeps still carry.
    pub left: u64,
}

/// How many delete list entries a frozen stripe holds before a round folds them,
/// `03-the-shape.md`.
pub const FOLD_LISTED: usize = 4_096;

/// How many times `wal_replay_budget` of debt the stripes may carry before a new write
/// transaction waits for a round, section 10.7.
pub const STALL_BUDGETS: u64 = 4;

/// Plans a round at cut `cut` over `stripes`, section 10.5 steps 2, 5 and 6, choosing stripes
/// until the debt left is under half of `budget`. `floors` are the checkpoint timestamps of what
/// else the log holds records for, the catalog and the key runs, which the log base may not pass.
#[must_use]
pub fn plan(stripes: &[Standing], cut: u64, budget: u64, floors: &[u64]) -> Round {
    let mut steps: Vec<(Step, u64)> = stripes
        .iter()
        .map(|stripe| match stripe.form {
            Form::Finished { at } if at <= cut => (Step::Publish, at),
            Form::Finished { .. } => (Step::Keep, stripe.checkpoint),
            Form::Frozen { listed } if listed > FOLD_LISTED => (Step::Fold, cut),
            _ if stripe.changed <= stripe.checkpoint => (Step::Bump, cut),
            _ => (Step::Keep, stripe.checkpoint),
        })
        .collect();
    let mut dirty: Vec<usize> = (0..stripes.len()).filter(|&i| steps[i].0 == Step::Keep).collect();
    let chosen = |stripe: &Standing| match stripe.form {
        Form::Hot => (Step::Write, cut),
        Form::Frozen { .. } => (Step::Fold, cut),
        Form::Finished { .. } => (Step::Keep, stripe.checkpoint),
    };
    let mut left: u64 = dirty.iter().map(|&i| stripes[i].debt).sum();
    dirty.sort_by_key(|&i| std::cmp::Reverse(stripes[i].debt));
    for &i in &dirty {
        if left.saturating_mul(2) < budget {
            break;
        }
        let step = chosen(&stripes[i]);
        if step.0 != Step::Keep {
            steps[i] = step;
            left -= stripes[i].debt;
        }
    }
    // The oldest C_s left behind goes too, so the log base moves on every round.
    let oldest = dirty
        .iter()
        .copied()
        .filter(|&i| steps[i].0 == Step::Keep && chosen(&stripes[i]).0 != Step::Keep)
        .min_by_key(|&i| stripes[i].checkpoint);
    if let Some(i) = oldest {
        steps[i] = chosen(&stripes[i]);
        left -= stripes[i].debt;
    }
    let base = steps.iter().map(|&(_, at)| at).chain(floors.iter().copied()).min().unwrap_or(cut);
    Round { cut, steps, base, left }
}

/// Whether a new write transaction waits for a round, because the stripes carry more debt than
/// [`STALL_BUDGETS`] times `budget`: the rounds are not keeping up with the writes.
#[must_use]
pub fn must_wait(debt: u64, budget: u64) -> bool {
    debt > budget.saturating_mul(STALL_BUDGETS)
}

/// A lane's log segment as the recycler sees it, section 10.7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// The largest commit timestamp in it.
    pub newest: u64,
    /// Transactions still running that spilled records into it.
    pub pinned: u32,
}

/// How many of a lane's segments, oldest first, can be recycled once the log base is `base`:
/// the leading run whose every commit is at or below the base and that no running transaction
/// spilled into. The first segment past them is where the lane's replay starts.
#[must_use]
pub fn recyclable(segments: &[Segment], base: u64) -> usize {
    segments.iter().take_while(|segment| segment.newest <= base && segment.pinned == 0).count()
}

#[cfg(test)]
mod tests {
    use super::{Form, Segment, Standing, Step, must_wait, plan, recyclable};

    fn hot(stripe: u32, checkpoint: u64, changed: u64, debt: u64) -> Standing {
        Standing { table: 1, stripe, checkpoint, changed, debt, form: Form::Hot }
    }

    #[test]
    fn a_clean_stripe_is_bumped_and_the_base_moves_to_the_cut() {
        let stripes = [hot(0, 10, 8, 0), hot(1, 20, 20, 0)];
        let round = plan(&stripes, 50, 1_000, &[]);
        assert_eq!(round.steps, [(Step::Bump, 50), (Step::Bump, 50)]);
        assert_eq!((round.base, round.left), (50, 0));
    }

    #[test]
    fn a_round_takes_the_largest_debts_until_under_half_the_budget() {
        // 205 of debt and a budget of 100: taking 100 and 50 leaves 55, which is not under half,
        // and 30 leaves 25. The oldest C_s left, stripe 4, goes as well.
        let stripes = [
            hot(0, 30, 40, 50),
            hot(1, 30, 40, 100),
            hot(2, 30, 40, 20),
            hot(3, 30, 40, 30),
            hot(4, 10, 40, 5),
            hot(5, 25, 25, 0),
        ];
        let round = plan(&stripes, 60, 100, &[]);
        let steps: Vec<Step> = round.steps.iter().map(|&(step, _)| step).collect();
        assert_eq!(
            steps,
            [Step::Write, Step::Write, Step::Keep, Step::Write, Step::Write, Step::Bump]
        );
        assert_eq!(round.left, 20);
        assert_eq!(round.base, 30, "stripe 2 still needs the log above its C_s");
    }

    #[test]
    fn the_oldest_checkpoint_goes_even_when_the_debt_is_under_budget() {
        let stripes = [hot(0, 5, 9, 1), hot(1, 7, 9, 2), hot(2, 9, 9, 0)];
        let round = plan(&stripes, 12, 1_000, &[]);
        assert_eq!(round.steps, [(Step::Write, 12), (Step::Keep, 7), (Step::Bump, 12)]);
        assert_eq!((round.base, round.left), (7, 2));
        // The next round takes the other one.
        let stripes = [hot(0, 12, 12, 0), hot(1, 7, 9, 2), hot(2, 12, 12, 0)];
        assert_eq!(plan(&stripes, 13, 1_000, &[]).base, 13);
    }

    #[test]
    fn frozen_stripes_fold_long_lists_and_finished_freezes_are_published() {
        let frozen = |listed| Form::Frozen { listed };
        let stripes = [
            Standing { form: frozen(5_000), ..hot(0, 10, 12, 0) },
            Standing { form: frozen(10), ..hot(1, 10, 10, 0) },
            Standing { form: frozen(10), ..hot(2, 10, 12, 1) },
            Standing { form: Form::Finished { at: 15 }, ..hot(3, 4, 15, 900) },
            Standing { form: Form::Finished { at: 25 }, ..hot(4, 4, 25, 900) },
        ];
        let round = plan(&stripes, 20, 1_000, &[]);
        assert_eq!(
            round.steps,
            [
                (Step::Fold, 20),
                (Step::Bump, 20),
                (Step::Fold, 20),
                (Step::Publish, 15),
                (Step::Keep, 4)
            ]
        );
        assert_eq!(round.base, 4, "a freeze past the cut keeps its stripe's C_s");
    }

    #[test]
    fn the_base_stays_at_or_under_the_floors() {
        let round = plan(&[hot(0, 10, 10, 0)], 30, 100, &[22, 40]);
        assert_eq!(round.base, 22);
        assert_eq!(plan(&[], 30, 100, &[]).base, 30);
    }

    #[test]
    fn segments_are_recycled_up_to_the_first_one_the_base_still_needs() {
        let segment = |newest, pinned| Segment { newest, pinned };
        let lane = [segment(10, 0), segment(20, 0), segment(15, 1), segment(18, 0), segment(40, 0)];
        assert_eq!(recyclable(&lane, 25), 2);
        assert_eq!(recyclable(&lane, 5), 0);
        assert_eq!(recyclable(&lane[3..], 25), 1);
        assert!(!must_wait(400, 100) && must_wait(401, 100));
    }
}
