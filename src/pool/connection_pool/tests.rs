//! Tests for ConnectionPool trait and MockConnectionPool

use crate::pool::{ConnectionPool, DeadpoolConnectionProvider, MockConnectionPool};

#[tokio::test]
async fn test_mock_connection_pool_creation() {
    let pool = MockConnectionPool::new("test-server");

    assert_eq!(pool.name(), "test-server");
    assert_eq!(pool.host(), "mock.example.com");
    assert_eq!(pool.port(), 119);
}

#[tokio::test]
async fn test_mock_connection_pool_with_custom_settings() {
    let pool = MockConnectionPool::new("custom-server")
        .with_host("custom.example.com")
        .with_port(563);

    assert_eq!(pool.name(), "custom-server");
    assert_eq!(pool.host(), "custom.example.com");
    assert_eq!(pool.port(), 563);
}

#[tokio::test]
async fn test_mock_connection_pool_status() {
    use crate::types::{AvailableConnections, CreatedConnections, MaxPoolSize};
    let pool = MockConnectionPool::new("test-server");
    let status = pool.status();

    assert_eq!(status.available, AvailableConnections::zero());
    assert_eq!(status.max_size, MaxPoolSize::new(0));
    assert_eq!(status.created, CreatedConnections::zero());
}

#[tokio::test]
async fn test_mock_connection_pool_get_fails() {
    let pool = MockConnectionPool::new("test-server");

    // Default mock implementation should fail with descriptive error
    let result = pool.get().await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("MockConnectionPool::get()"));
}

#[tokio::test]
async fn test_deadpool_connection_provider_implements_trait() {
    use crate::types::MaxPoolSize;
    // Test that DeadpoolConnectionProvider implements ConnectionPool trait
    let provider = DeadpoolConnectionProvider::builder("news.example.com", 119)
        .name("Test Server")
        .max_connections(5)
        .build()
        .unwrap();

    // Access via trait methods
    assert_eq!(provider.name(), "Test Server");
    assert_eq!(provider.host(), "news.example.com");
    assert_eq!(provider.port(), 119);

    let status = provider.status();
    assert_eq!(status.max_size, MaxPoolSize::new(5));
}

#[tokio::test]
async fn test_connection_pool_trait_is_object_safe() {
    // Verify we can use ConnectionPool as a trait object
    let provider = DeadpoolConnectionProvider::builder("news.example.com", 119)
        .name("Dynamic Server")
        .max_connections(3)
        .build()
        .unwrap();

    // Can be boxed as trait object (Arc for shared ownership)
    let boxed: std::sync::Arc<dyn ConnectionPool> = std::sync::Arc::new(provider);
    assert_eq!(boxed.name(), "Dynamic Server");
}

/// Helper function demonstrating generic usage of ConnectionPool trait
async fn use_any_pool<P: ConnectionPool>(pool: &P) -> String {
    format!("{}://{}:{}", pool.name(), pool.host(), pool.port())
}

#[tokio::test]
async fn test_connection_pool_generic_usage() {
    let real_pool = DeadpoolConnectionProvider::builder("real.example.com", 119)
        .name("Real")
        .build()
        .unwrap();

    let mock_pool = MockConnectionPool::new("Mock")
        .with_host("mock.example.com")
        .with_port(563);

    // Both can be used with the same generic function
    let real_info = use_any_pool(&real_pool).await;
    let mock_info = use_any_pool(&mock_pool).await;

    assert_eq!(real_info, "Real://real.example.com:119");
    assert_eq!(mock_info, "Mock://mock.example.com:563");
}
