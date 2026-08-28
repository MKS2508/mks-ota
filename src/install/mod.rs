//! Installation strategies. `full` replaces the whole app package and
//! relaunches it; `partial` (F2) swaps a single artifact inside
//! `appDataDir` without touching the bundle — separate paths, not a flag
//! (ADR-0045 D8-C).

pub mod full;
pub mod partial;
