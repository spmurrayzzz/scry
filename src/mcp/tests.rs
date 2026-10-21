use super::*;
use scry::Config;
use tokio::sync::TryAcquireError;

#[tokio::test]
async fn default_operations_limiter_shares_twenty_slots_across_handler_clones() {
    let service = Scry::new(Config::default()).unwrap();
    assert_eq!(service.config().limits.max_requests, 20);
    let handler = Mcp::new(service, CancellationToken::new());
    let clone = handler.clone();
    assert_eq!(handler.operations.available_permits(), 20);
    let mut pending = Vec::new();
    for index in 0..20 {
        let operations = if index % 2 == 0 {
            &handler.operations
        } else {
            &clone.operations
        };
        pending.push(operations.clone().try_acquire_owned().unwrap());
        assert_eq!(handler.operations.available_permits(), 19 - index);
    }
    for handler in [&handler, &clone] {
        assert!(matches!(
            handler.operations.try_acquire(),
            Err(TryAcquireError::NoPermits)
        ));
    }
    drop(pending.pop().unwrap());
    assert_eq!(handler.operations.available_permits(), 1);
    let replacement = clone.operations.try_acquire().unwrap();
    assert!(matches!(
        handler.operations.try_acquire(),
        Err(TryAcquireError::NoPermits)
    ));
    drop(replacement);
    drop(pending);
    assert_eq!(handler.operations.available_permits(), 20);
    assert_eq!(clone.operations.available_permits(), 20);
}
