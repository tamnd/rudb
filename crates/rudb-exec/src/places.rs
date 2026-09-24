//! The aggregate's map of places to slots, and the pool the maps go back to.
//!
//! Every instance of a grouping sink seeds a map on the key's ends before it has read a row, up to
//! a quarter of a million places, and clearing it was the largest single cost an instance paid
//! before its first chunk. On ClickBench 28 at six threads it was 120 of 1416 samples, nearly all of
//! it the kernel handing out fresh pages for the clear to write to. The map a finished instance
//! drops is the same size as the one the next instance asks for, so it is kept here instead, with
//! only the places it wrote set back, and the next instance takes it as it is.
//!
//! The places written are few next to the map. A group is written to the map once, when the row
//! that found it missed, so an instance that saw four thousand groups sets four thousand places
//! back rather than clearing a quarter of a million. A map that wrote more than a sixteenth of its
//! places is cleared whole, which is still no new pages.

use std::ops::Deref;
use std::sync::Mutex;

use crate::table::UNSEEN;

/// How many maps the pool keeps. One per thread that folds at once is all it ever needs, and a
/// map past this is freed the ordinary way.
const KEPT: usize = 64;

/// The widest map the pool takes back, four million places or 16 MB, so that one query with a very
/// wide key does not leave that much behind for the life of the process.
const WIDEST: usize = 1 << 22;

/// Maps dropped by instances that finished, every place [`UNSEEN`].
static POOL: Mutex<Vec<Vec<u32>>> = Mutex::new(Vec::new());

/// One slot per place, or [`UNSEEN`], which remembers which places it wrote.
///
/// It reads as a slice, and writes only through [`Places::set`], so that nothing reaches a place
/// without it being counted.
#[derive(Debug, Default)]
pub(crate) struct Places {
    map: Vec<u32>,
    /// The places written since the map was last all [`UNSEEN`], while there are few enough of
    /// them to be worth setting back one at a time.
    written: Vec<u32>,
    /// Set when `written` stopped counting, so that the whole map is cleared instead.
    crowded: bool,
}

impl Places {
    /// A map of `len` places, every one [`UNSEEN`], taken from the pool when it has one.
    pub(crate) fn seeded(len: usize) -> Self {
        let mut map = POOL.lock().ok().and_then(|mut pool| pool.pop()).unwrap_or_default();
        map.resize(len, UNSEEN);
        Self { map, written: Vec::new(), crowded: false }
    }

    /// Writes `held` at `place`.
    #[inline(always)]
    pub(crate) fn set(&mut self, place: usize, held: u32) {
        self.map[place] = held;
        if !self.crowded {
            match u32::try_from(place) {
                Ok(place) if self.written.len() < self.map.len() / 16 => self.written.push(place),
                _ => self.crowded = true,
            }
        }
    }

    /// Makes the map `len` places, every one [`UNSEEN`].
    pub(crate) fn reset(&mut self, len: usize) {
        self.map.clear();
        self.map.resize(len, UNSEEN);
        self.written.clear();
        self.crowded = false;
    }

    /// Widens the map from `span` places to `len`, keeping every place but the last, the null
    /// place, which moves to the new last place.
    pub(crate) fn widen(&mut self, span: usize, len: usize) {
        let null = std::mem::replace(&mut self.map[span - 1], UNSEEN);
        self.map.resize(len, UNSEEN);
        self.set(len - 1, null);
    }

    /// Sets every written place back to [`UNSEEN`].
    fn clean(&mut self) {
        if self.crowded {
            self.map.fill(UNSEEN);
        } else {
            for &place in &self.written {
                self.map[place as usize] = UNSEEN;
            }
        }
    }
}

impl Deref for Places {
    type Target = [u32];

    fn deref(&self) -> &[u32] {
        &self.map
    }
}

impl Drop for Places {
    fn drop(&mut self) {
        if self.map.is_empty() || self.map.len() > WIDEST {
            return;
        }
        self.clean();
        debug_assert!(self.map.iter().all(|&held| held == UNSEEN));
        if let Ok(mut pool) = POOL.lock()
            && pool.len() < KEPT
        {
            pool.push(std::mem::take(&mut self.map));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_map_comes_back_clear() {
        let mut places = Places::seeded(1_000);
        places.set(3, 7);
        places.set(999, 8);
        places.widen(1_000, 1_200);
        assert_eq!(places[1_199], 8, "the null place moved to the new end");
        assert_eq!(places[999], UNSEEN);
        drop(places);
        // Other tests share the pool, so every map in it is checked rather than the one just given.
        for map in POOL.lock().unwrap().iter() {
            assert!(map.iter().all(|&held| held == UNSEEN));
        }
        let places = Places::seeded(500);
        assert_eq!(places.len(), 500);
        assert!(places.iter().all(|&held| held == UNSEEN));
    }

    #[test]
    fn a_crowded_map_is_cleared_whole() {
        let mut places = Places::seeded(64);
        for place in 0..64 {
            places.set(place, place as u32);
        }
        assert!(places.crowded);
        places.clean();
        assert!(places.iter().all(|&held| held == UNSEEN));
    }
}
