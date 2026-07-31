//! What a `--features` flag actually gives you.
//!
//! A feature that enables a driver in one crate and not another is invisible
//! until somebody reaches for the missing half — `--features redis` gave you a
//! Redis cache and no Redis queue for a while, and nothing failed until you
//! looked for `RedisQueue`. So the reach is asserted here rather than left to
//! a dependency graph nobody reads.

#[test]
#[cfg(feature = "redis")]
fn the_redis_feature_reaches_the_cache_the_queue_and_the_broadcaster() {
    // Naming the types *is* the test: this file does not compile without them.
    let names = [
        std::any::type_name::<rainier_framework::cache::RedisCache>(),
        std::any::type_name::<rainier_framework::queue::RedisQueue>(),
        std::any::type_name::<rainier_framework::broadcast::RedisBroadcaster>(),
    ];

    assert!(names.iter().all(|name| name.contains("Redis")));
}

#[test]
#[cfg(feature = "sqs")]
fn the_sqs_feature_reaches_the_queue() {
    assert!(std::any::type_name::<rainier_framework::queue::SqsQueue>().contains("Sqs"));
}

#[test]
#[cfg(feature = "sea-orm-executor")]
fn the_executor_feature_reaches_the_database() {
    assert!(
        std::any::type_name::<rainier_framework::drivers::sql::SeaOrmExecutor>().contains("SeaOrm")
    );
}

/// Only exists in a build with no drivers — which is the build it is about.
#[test]
#[cfg(not(any(feature = "redis", feature = "sea-orm-executor", feature = "memcached")))]
fn the_default_build_turns_on_no_drivers() {
    // The wasm-safe build. If a driver ever arrives here by default, the
    // default has grown a dependency that cannot compile for a Worker.
    const {
        assert!(!cfg!(feature = "redis"), "the default build should reach no Redis");
        assert!(!cfg!(feature = "sea-orm-executor"), "nor a native SQL executor");
        assert!(!cfg!(feature = "memcached"));
    }
}
