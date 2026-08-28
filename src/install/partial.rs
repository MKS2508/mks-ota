//! Partial install — a single artifact (frontend bundle, sidecar, assets,
//! hooks/skills) swapped without reinstalling the app — F2 scope, not
//! implemented in F1.
//!
//! Not written from scratch: extracted from `tpv-el-haido2`'s existing A/B
//! slot updater (`src-tauri/src/ota/`, 1514 LOC, 43 tests), which already
//! solves this (`docs/jarvis/ota-crate-design-2026-08-28.md` §2). Target
//! shape once extracted:
//!
//! - artifact lives in `appDataDir`, outside the signed bundle (L1 —
//!   writing inside a signed `.app`/AppImage breaks the seal);
//! - slots A/B with a pointer file, not a symlink (Windows can't create
//!   symlinks without privileges);
//! - stage (decompress, slow) and activate (swap the pointer, instant) are
//!   two separate steps;
//! - `invalidate_if_native_changed()` — after a full-install swaps the
//!   native binary, a stale partial slot must not keep serving an old
//!   frontend against new native commands. This invariant is part of the
//!   crate's contract from F1 even though the partial path itself lands in
//!   F2 (design §2, L6).
