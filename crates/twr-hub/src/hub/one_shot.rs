//! What the hub's one-shot CLI calls share (next-message suggestions, `hub::suggest`, and a
//! loop's review runs on the hub's own box): `twr_runner::one_shot`, which every box's review
//! runs use too.

pub(super) use twr_runner::one_shot::{kill_group, scrub_env, Group};
