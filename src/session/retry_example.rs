pub(super) async fn run() {
    let mut attempts = 0;
    let result = super::retry_once!(
        async {
            attempts += 1;
            if attempts == 1 {
                Err("stale connection")
            } else {
                Ok("ready")
            }
        }
        .await,
        backend = "example"
    );
    assert_eq!(result, Ok("ready"));
    assert_eq!(attempts, 2);

    let mut attempts = 0;
    let result: Result<(), usize> = super::retry_once!(
        async {
            attempts += 1;
            Err(attempts)
        }
        .await
    );
    assert_eq!(result, Err(1));
    assert_eq!(attempts, 2);
}
