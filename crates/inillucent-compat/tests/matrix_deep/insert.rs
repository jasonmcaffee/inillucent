//! The `insert` family of the statement matrix, at the cadence of every merge.
//!
//! Invariant: **every case of the family runs on files on both engines and is
//! graded against the pinned SQLite, and the group fails once, naming every
//! case that disagreed and every `known.list` line that no longer does.** The
//! cases and the grading are in `inillucent_compat::statement_matrix`; this
//! file only says which family and which cadence, so a new family is one line.

inillucent_compat::matrix_family!(insert, Merge, [g0, g1, g2, g3, g4, g5, g6, g7]);
