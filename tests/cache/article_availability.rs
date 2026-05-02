//! Tests for `ArticleAvailability` bitset semantics and retry logic.

use nntp_proxy::cache::ArticleAvailability;
use nntp_proxy::router::BackendCount;
use nntp_proxy::types::BackendId;

fn backend(index: usize) -> BackendId {
    BackendId::from_index(index)
}

fn count(count: usize) -> BackendCount {
    BackendCount::new(count)
}

fn availability_with_missing(backends: &[usize]) -> ArticleAvailability {
    let mut avail = ArticleAvailability::new();
    backends.iter().for_each(|backend_index| {
        avail.record_missing(backend(*backend_index));
    });
    avail
}

fn assert_should_try(avail: &ArticleAvailability, cases: &[(usize, bool)]) {
    cases.iter().for_each(|(backend_index, should_try)| {
        assert_eq!(
            avail.should_try(backend(*backend_index)),
            *should_try,
            "backend {backend_index}"
        );
    });
}

#[test]
fn test_should_try_tracks_missing_backends() {
    let fresh = ArticleAvailability::new();
    assert_should_try(
        &fresh,
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
        &avail,
        &[
            (0, false),
            (1, true),
            (2, false),
            (3, true),
            (4, false),
            (5, true),
        ],
    );
    assert!(!avail.all_exhausted(count(6)));
}

#[test]
fn test_record_missing_is_idempotent_and_encoded_as_bitset() {
    let mut avail = ArticleAvailability::new();
    assert_eq!(avail.as_u8(), 0b0000_0000);

    avail.record_missing(backend(0));
    assert_eq!(avail.as_u8(), 0b0000_0001);

    avail.record_missing(backend(1));
    assert_eq!(avail.as_u8(), 0b0000_0011);

    avail.record_missing(backend(3));
    avail.record_missing(backend(3));
    assert_eq!(avail.as_u8(), 0b0000_1011);
    assert!(!avail.should_try(backend(3)));

    avail.record_missing(backend(7));
    assert_eq!(avail.as_u8(), 0b1000_1011);
}

#[test]
fn test_all_exhausted_for_backend_counts() {
    assert!(ArticleAvailability::new().all_exhausted(count(0)));

    let mut one = ArticleAvailability::new();
    assert!(!one.all_exhausted(count(1)));
    one.record_missing(backend(0));
    assert!(one.all_exhausted(count(1)));

    let mut two = ArticleAvailability::new();
    assert!(!two.all_exhausted(count(2)));
    two.record_missing(backend(0));
    assert!(!two.all_exhausted(count(2)));
    two.record_missing(backend(1));
    assert!(two.all_exhausted(count(2)));

    let mut eight = ArticleAvailability::new();
    (0..7).for_each(|backend_index| {
        eight.record_missing(backend(backend_index));
        assert!(!eight.all_exhausted(count(8)));
    });
    eight.record_missing(backend(7));
    assert!(eight.all_exhausted(count(8)));
}

#[test]
fn test_retry_loop_simulations() {
    let mut exhausted = ArticleAvailability::new();
    let mut attempts = Vec::new();
    [0, 1].into_iter().for_each(|backend_index| {
        let backend = backend(backend_index);
        assert!(exhausted.should_try(backend));
        attempts.push(backend);
        exhausted.record_missing(backend);
    });
    assert!(exhausted.all_exhausted(count(2)));
    assert_eq!(attempts, vec![backend(0), backend(1)]);

    let mut found = ArticleAvailability::new();
    [0, 1].into_iter().for_each(|backend_index| {
        let backend = backend(backend_index);
        assert!(found.should_try(backend));
        found.record_missing(backend);
    });
    assert!(found.should_try(backend(2)));
    assert!(!found.all_exhausted(count(4)));
    assert_should_try(&found, &[(0, false), (1, false), (2, true), (3, true)]);
}

#[test]
fn test_round_robin_and_cached_availability_skip_missing_backends() {
    let avail = availability_with_missing(&[1, 3, 5]);

    let tried = [0, 1, 2, 3, 0]
        .into_iter()
        .map(backend)
        .filter(|&backend| avail.should_try(backend))
        .collect::<Vec<_>>();
    assert_eq!(tried, vec![backend(0), backend(2), backend(0)]);

    assert_should_try(
        &avail,
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
            .filter(|&backend| avail.should_try(backend))
            .collect::<Vec<_>>(),
        vec![backend(0), backend(2), backend(4)]
    );
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "Backend count 9 exceeds MAX_BACKENDS")]
fn test_all_exhausted_panics_with_nine_backends() {
    let avail = availability_with_missing(&(0..8).collect::<Vec<_>>());
    let _ = avail.all_exhausted(count(9));
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "Backend index 8 exceeds MAX_BACKENDS")]
fn test_backend_id_out_of_range_panics() {
    let _ = availability_with_missing(&[8]);
}
