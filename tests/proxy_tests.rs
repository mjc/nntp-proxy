// Suppress pedantic/nursery lints in integration tests — not worth documenting helper functions.
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]
#![allow(clippy::float_cmp)]
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::items_after_statements)]

mod test_helpers;

mod proxy;
