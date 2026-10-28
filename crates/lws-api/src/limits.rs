//! Per-client token-bucket rate limiting.
//!
//! Buckets are sharded so the limiter is not a global lock under load, and
//! idle buckets are swept so memory tracks active clients only. IPv6 clients
//! are bucketed by /64, the smallest prefix a single subscriber controls,
//! so rotating addresses inside one allocation does not reset the limit.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const SHARDS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClientKey(u128);

impl From<IpAddr> for ClientKey {
    fn from(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(v4) => ClientKey(u128::from(u32::from(v4))),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => ClientKey(u128::from(u32::from(v4))),
                None => ClientKey((u128::from(v6) >> 64) << 64 | 1),
            },
        }
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

pub struct RateLimiter {
    rate: f64,
    burst: f64,
    shards: Vec<Mutex<HashMap<ClientKey, Bucket>>>,
}

impl RateLimiter {
    /// `rate` tokens per second, up to `burst` saved. A rate of 0 disables it.
    pub fn new(rate: f64, burst: f64) -> RateLimiter {
        RateLimiter { rate, burst: burst.max(1.0), shards: (0..SHARDS).map(|_| Mutex::default()).collect() }
    }

    pub fn enabled(&self) -> bool {
        self.rate > 0.0
    }

    fn shard(&self, key: &ClientKey) -> &Mutex<HashMap<ClientKey, Bucket>> {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut h);
        &self.shards[(h.finish() as usize) % SHARDS]
    }

    /// Takes `cost` tokens. On refusal returns the seconds until it would pass.
    pub fn check(&self, key: ClientKey, cost: f64) -> Result<(), u64> {
        if !self.enabled() {
            return Ok(());
        }
        let now = Instant::now();
        let mut shard = self.shard(&key).lock().unwrap();
        let bucket = shard.entry(key).or_insert(Bucket { tokens: self.burst, last: now });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rate).min(self.burst);
        bucket.last = now;
        if bucket.tokens >= cost {
            bucket.tokens -= cost;
            Ok(())
        } else {
            Err(((cost - bucket.tokens) / self.rate).ceil() as u64)
        }
    }

    /// Drops buckets that have been idle long enough to be full again.
    pub fn sweep(&self) {
        let full_after =
            Duration::from_secs_f64(self.burst / self.rate.max(f64::MIN_POSITIVE)).min(Duration::from_secs(3600));
        let now = Instant::now();
        for shard in &self.shards {
            shard.lock().unwrap().retain(|_, b| now.duration_since(b.last) < full_after);
        }
    }

    pub fn tracked(&self) -> usize {
        self.shards.iter().map(|s| s.lock().unwrap().len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_and_refills() {
        let l = RateLimiter::new(1000.0, 3.0);
        let k = ClientKey::from("10.0.0.1".parse::<IpAddr>().unwrap());
        assert!(l.check(k, 1.0).is_ok());
        assert!(l.check(k, 1.0).is_ok());
        assert!(l.check(k, 1.0).is_ok());
        assert!(l.check(k, 1.0).is_err());
        std::thread::sleep(Duration::from_millis(5));
        assert!(l.check(k, 1.0).is_ok());
        let other = ClientKey::from("10.0.0.2".parse::<IpAddr>().unwrap());
        assert!(l.check(other, 1.0).is_ok());
    }

    #[test]
    fn ipv6_is_bucketed_by_prefix() {
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(ClientKey::from(a), ClientKey::from(b));
        assert_ne!(ClientKey::from(a), ClientKey::from(c));
        let mapped: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        assert_eq!(ClientKey::from(mapped), ClientKey::from("10.0.0.1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn sweep_forgets_idle_clients() {
        let l = RateLimiter::new(1_000_000.0, 1.0);
        l.check(ClientKey(1), 1.0).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        l.sweep();
        assert_eq!(l.tracked(), 0);
    }
}
