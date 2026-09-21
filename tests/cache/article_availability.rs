//! Tests for `ArticleAvailability` bitset semantics and retry logic.

use nntp_proxy::cache::{ArticleAvailability, AvailabilityMask, AvailabilitySlot};
use nntp_proxy::types::BackendId;

fn backend(index: usize) -> BackendId {
    BackendId::from_index(index)
}

fn mask(count: usize) -> AvailabilityMask {
    let slots = (0..count)
        .map(|index| AvailabilitySlot::new(index).unwrap())
        .collect::<Vec<_>>();
    AvailabilityMask::from_slots(&slots)
}

fn availability_with_missing(backends: &[usize]) -> ArticleAvailability {
    let mut avail = ArticleAvailability::new();
    for backend_index in backends {
        avail.record_missing_slot(AvailabilitySlot::new(*backend_index).unwrap());
    }
    avail
}

fn assert_should_try(avail: ArticleAvailability, cases: &[(usize, bool)]) {
    for (backend_index, should_try) in cases {
        assert_eq!(
            !avail.is_missing_slot(AvailabilitySlot::new(*backend_index).unwrap()),
            *should_try,
            "backend {backend_index}"
        );
    }
}

#[test]
fn test_should_try_tracks_missing_backends() {
    let fresh = ArticleAvailability::new();
    assert_should_try(
        fresh,
        &[
            (0, true),
            (1, true),
            (2, true),
            (3, true),
            (4, true),
            (5, true),
            (6, true),
            (7, true),
        ],
    );

    let avail = availability_with_missing(&[0, 2, 4]);
    assert_should_try(
        avail,
        &[
            (0, false),
            (1, true),
            (2, false),
            (3, true),
            (4, false),
            (5, true),
        ],
    );
    assert!(!avail.all_exhausted(mask(6)));
}

#[test]
fn test_record_missing_is_idempotent_and_encoded_as_bitset() {
    let mut avail = ArticleAvailability::new();
    assert_eq!(avail.missing_bits(), 0b0000_0000);

    avail.record_missing_slot(AvailabilitySlot::new(0).unwrap());
    assert_eq!(avail.missing_bits(), 0b0000_0001);

    avail.record_missing_slot(AvailabilitySlot::new(1).unwrap());
    assert_eq!(avail.missing_bits(), 0b0000_0011);

    avail.record_missing_slot(AvailabilitySlot::new(3).unwrap());
    avail.record_missing_slot(AvailabilitySlot::new(3).unwrap());
    assert_eq!(avail.missing_bits(), 0b0000_1011);
    assert!(avail.is_missing_slot(AvailabilitySlot::new(3).unwrap()));

    avail.record_missing_slot(AvailabilitySlot::new(7).unwrap());
    assert_eq!(avail.missing_bits(), 0b1000_1011);
}

#[test]
fn test_all_exhausted_for_provider_slots() {
    assert!(ArticleAvailability::new().all_exhausted(mask(0)));

    let mut one = ArticleAvailability::new();
    assert!(!one.all_exhausted(mask(1)));
    one.record_missing_slot(AvailabilitySlot::new(0).unwrap());
    assert!(one.all_exhausted(mask(1)));

    let mut two = ArticleAvailability::new();
    assert!(!two.all_exhausted(mask(2)));
    two.record_missing_slot(AvailabilitySlot::new(0).unwrap());
    assert!(!two.all_exhausted(mask(2)));
    two.record_missing_slot(AvailabilitySlot::new(1).unwrap());
    assert!(two.all_exhausted(mask(2)));

    let mut eight = ArticleAvailability::new();
    for backend_index in 0..7 {
        eight.record_missing_slot(AvailabilitySlot::new(backend_index).unwrap());
        assert!(!eight.all_exhausted(mask(8)));
    }
    eight.record_missing_slot(AvailabilitySlot::new(7).unwrap());
    assert!(eight.all_exhausted(mask(8)));
}

#[test]
fn test_retry_loop_simulations() {
    let mut exhausted = ArticleAvailability::new();
    let mut attempts = Vec::new();
    for backend_index in [0, 1] {
        let backend = backend(backend_index);
        assert!(!exhausted.is_missing_slot(AvailabilitySlot::new(backend.as_index()).unwrap()));
        attempts.push(backend);
        exhausted.record_missing_slot(AvailabilitySlot::new(backend.as_index()).unwrap());
    }
    assert!(exhausted.all_exhausted(mask(2)));
    assert_eq!(attempts, vec![backend(0), backend(1)]);

    let mut found = ArticleAvailability::new();
    for backend_index in [0, 1] {
        let backend = backend(backend_index);
        assert!(!found.is_missing_slot(AvailabilitySlot::new(backend.as_index()).unwrap()));
        found.record_missing_slot(AvailabilitySlot::new(backend.as_index()).unwrap());
    }
    assert!(!found.is_missing_slot(nntp_proxy::cache::AvailabilitySlot::new(2).unwrap()));
    assert!(!found.all_exhausted(mask(4)));
    assert_should_try(found, &[(0, false), (1, false), (2, true), (3, true)]);
}

#[test]
fn test_round_robin_and_cached_availability_skip_missing_backends() {
    let avail = availability_with_missing(&[1, 3, 5]);

    let tried = [0, 1, 2, 3, 0]
        .into_iter()
        .map(backend)
        .filter(|&backend| {
            !avail.is_missing_slot(
                nntp_proxy::cache::AvailabilitySlot::new(backend.as_index()).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(tried, vec![backend(0), backend(2), backend(0)]);

    assert_should_try(
        avail,
        &[
            (0, true),
            (1, false),
            (2, true),
            (3, false),
            (4, true),
            (5, false),
        ],
    );
    assert_eq!(
        (0..6)
            .map(backend)
            .filter(|&backend| !avail
                .is_missing_slot(AvailabilitySlot::new(backend.as_index()).unwrap()))
            .collect::<Vec<_>>(),
        vec![backend(0), backend(2), backend(4)]
    );
}

#[test]
fn test_all_exhausted_supports_nine_backends() {
    let avail = availability_with_missing(&(0..8).collect::<Vec<_>>());
    assert!(!avail.all_exhausted(mask(9)));
}

#[test]
fn exhaustion_uses_provider_slots_not_transport_backend_count() {
    let slot_five = AvailabilitySlot::new(5).unwrap();
    let configured = AvailabilityMask::from_slots(&[slot_five, slot_five]);

    let mut avail = ArticleAvailability::new();
    avail.record_missing_slot(slot_five);

    assert!(avail.all_exhausted(configured));

    let mut unrelated = ArticleAvailability::new();
    unrelated.record_missing_slot(AvailabilitySlot::new(0).unwrap());
    assert!(!unrelated.all_exhausted(configured));
}

#[test]
fn test_backend_id_eight_is_supported() {
    let avail = availability_with_missing(&[8]);
    assert!(avail.is_missing_slot(AvailabilitySlot::new(8).unwrap()));
}
