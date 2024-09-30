use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct RateLimiter {
    limit: Duration,
    last_limit_reached: Instant,
}

impl RateLimiter {
    pub fn new(limit: Duration) -> Self {
        Self {
            limit,
            last_limit_reached: Instant::now() - limit,
        }
    }

    pub fn reset(&mut self) {
        self.last_limit_reached = Instant::now() - self.limit;
    }

    /// Returns true if the limit has been reached
    pub fn check(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.last_limit_reached) >= self.limit {
            self.last_limit_reached = now;
            true
        } else {
            false
        }
    }
}
