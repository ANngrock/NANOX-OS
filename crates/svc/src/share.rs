//! CPU shares for the interactive part and the services: weighted max-min
//! fair allocation with reservations and caps, in permille of the machine.
//!
//! Each claimant has a reservation (guaranteed while it wants that much),
//! a weight (its share of what is left), a cap and a current demand. The
//! result satisfies, with integer arithmetic and no floating point:
//!
//! 1. `alloc >= min(reserve, bound)` — the guarantee;
//! 2. `alloc <= bound` where `bound = min(cap, demand)`;
//! 3. `sum(alloc) <= TOTAL`;
//! 4. work conservation: if anyone with a positive weight can still take
//!    more, the whole machine is handed out;
//! 5. fairness: among claimants that could take more, the extra above the
//!    guarantee is proportional to the weight (within rounding).
//!
//! This is the policy the interactive-latency guarantee of `hybrid` mode
//! rests on: the interactive claimant's reservation cannot be taken by
//! services, whatever they demand.

pub const TOTAL: u32 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claim {
    pub reserve: u32,
    pub weight: u32,
    pub cap: u32,
    pub demand: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareError {
    /// The guarantees alone exceed the machine.
    Overcommitted,
    /// `out` is shorter than `claims`.
    Buffer,
}

impl Claim {
    fn bound(&self) -> u32 {
        self.cap.min(self.demand).min(TOTAL)
    }

    fn floor(&self) -> u32 {
        self.reserve.min(self.bound())
    }
}

/// Fills `out[..claims.len()]` with each claimant's share.
pub fn allocate(claims: &[Claim], out: &mut [u32]) -> Result<(), ShareError> {
    let n = claims.len();
    if out.len() < n {
        return Err(ShareError::Buffer);
    }
    let mut left = TOTAL;
    for (c, o) in claims.iter().zip(out.iter_mut()) {
        *o = c.floor();
        left = left.checked_sub(*o).ok_or(ShareError::Overcommitted)?;
    }
    // Water-filling: repeatedly split what is left by weight among those
    // that can still take more; anyone whose bound is reached drops out and
    // the rest is split again.
    loop {
        let mut weight_sum: u64 = 0;
        let mut active = 0;
        for (c, o) in claims.iter().zip(out.iter()) {
            if c.weight > 0 && *o < c.bound() {
                weight_sum += u64::from(c.weight);
                active += 1;
            }
        }
        if left == 0 || active == 0 {
            return Ok(());
        }
        // Does anyone hit its bound with a proportional share?
        let mut capped_any = false;
        for (c, o) in claims.iter().zip(out.iter_mut()) {
            if c.weight > 0 && *o < c.bound() {
                let share = (u64::from(left) * u64::from(c.weight) / weight_sum) as u32;
                let room = c.bound() - *o;
                if share >= room {
                    left -= room;
                    *o += room;
                    capped_any = true;
                    break; // recompute the split without it
                }
            }
        }
        if capped_any {
            continue;
        }
        // Nobody is capped: hand out the proportional shares, then the
        // rounding remainder one unit at a time in index order.
        let mut given = 0;
        for (c, o) in claims.iter().zip(out.iter_mut()) {
            if c.weight > 0 && *o < c.bound() {
                let share = (u64::from(left) * u64::from(c.weight) / weight_sum) as u32;
                *o += share;
                given += share;
            }
        }
        let mut rest = left - given;
        for (c, o) in claims.iter().zip(out.iter_mut()) {
            if rest == 0 {
                break;
            }
            if c.weight > 0 && *o < c.bound() {
                *o += 1;
                rest -= 1;
            }
        }
        return Ok(());
    }
}
