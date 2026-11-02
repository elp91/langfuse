//! Per-connection circuit breaker for fallback targets.
//!
//! A target that keeps failing spends the next requests' failover budget on
//! connect timeouts. The breaker opens after consecutive failures, stays open
//! for a cooldown, then admits one probe request; success resets it.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct BreakerConfig {
    /// Consecutive failures that open the breaker.
    pub threshold: u32,
    /// How long an open breaker refuses a target.
    pub cooldown: Duration,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            threshold: 3,
            cooldown: Duration::from_secs(30),
        }
    }
}

#[derive(Debug)]
enum State {
    /// Serving requests; consecutive failures are counted.
    Closed { failures: u32 },
    /// Refusing requests until `reopen_at`; failures past this point are not
    /// counted (the target is already presumed down).
    Open { reopen_at: Instant },
    /// The cooldown elapsed and one probe was admitted. Another failure
    /// re-opens; success closes.
    HalfOpen,
}

#[derive(Debug, Default)]
struct Breakers {
    by_connection: HashMap<String, State>,
}

/// Shared breaker state across concurrent requests. Lock hold times are
/// microseconds; the lock is never held across an await.
#[derive(Debug)]
pub(crate) struct CircuitBreakers {
    config: BreakerConfig,
    breakers: Mutex<Breakers>,
}

impl Default for CircuitBreakers {
    fn default() -> Self {
        Self::new(BreakerConfig::default())
    }
}

impl CircuitBreakers {
    pub(crate) fn new(config: BreakerConfig) -> Self {
        Self {
            config,
            breakers: Mutex::new(Breakers::default()),
        }
    }

    /// Whether a target may be attempted now. A half-open breaker admits one
    /// probe and stays half-open: concurrent probes are harmless retries.
    pub(crate) fn allows(&self, connection_id: &str) -> bool {
        let mut breakers = self.lock();
        let Some(state) = breakers.by_connection.get_mut(connection_id) else {
            return true;
        };
        match state {
            State::Closed { .. } | State::HalfOpen => true,
            State::Open { reopen_at } => {
                if Instant::now() >= *reopen_at {
                    *state = State::HalfOpen;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Record a failure. The breaker opens once failures reach the threshold;
    /// a half-open probe that fails re-opens for another cooldown.
    pub(crate) fn record_failure(&self, connection_id: &str) {
        let mut breakers = self.lock();
        let failures = match breakers.by_connection.get_mut(connection_id) {
            Some(State::Closed { failures }) => *failures + 1,
            Some(State::HalfOpen) => self.config.threshold,
            // The first failure already counts toward the threshold.
            Some(State::Open { .. }) | None => 1,
        };
        if failures >= self.config.threshold {
            breakers.by_connection.insert(
                connection_id.to_owned(),
                State::Open {
                    reopen_at: Instant::now() + self.config.cooldown,
                },
            );
        } else {
            breakers
                .by_connection
                .insert(connection_id.to_owned(), State::Closed { failures });
        }
    }

    /// Record a success: the target is healthy again.
    pub(crate) fn record_success(&self, connection_id: &str) {
        let mut breakers = self.lock();
        breakers
            .by_connection
            .insert(connection_id.to_owned(), State::Closed { failures: 0 });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Breakers> {
        self.breakers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests;
