use std::{
    net::IpAddr,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use dashmap::DashMap;

#[derive(Default)]
struct Windows {
    sec: (u64, u32),
    min: (u64, u32),
    day: (u64, u32),
}

pub struct RateLimiter {
    per_sec: u32,
    per_min: u32,
    per_day: u32,
    buckets: DashMap<IpAddr, Mutex<Windows>>,
}

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

impl RateLimiter {
    pub fn from_env() -> Self {
        Self {
            per_sec: env_u32("RATE_PER_SEC", 100),
            per_min: env_u32("RATE_PER_MIN", 1_000),
            per_day: env_u32("RATE_PER_DAY", 100_000),
            buckets: DashMap::new(),
        }
    }

    /// IPv6 is limited per /64 (interface bits are trivially rotated by a
    /// single client), IPv4 per address. IPv4-mapped IPv6 addresses
    /// (::ffff:a.b.c.d, seen on dual-stack binds) are unmapped first so they
    /// don't all collapse into one /64 bucket.
    fn key(ip: IpAddr) -> IpAddr {
        match ip {
            IpAddr::V4(_) => ip,
            IpAddr::V6(v6) => {
                if let Some(v4) = v6.to_ipv4_mapped() {
                    return IpAddr::V4(v4);
                }
                let mut octets = v6.octets();
                octets[8..].fill(0);
                IpAddr::V6(octets.into())
            }
        }
    }

    pub fn check(&self, ip: IpAddr) -> bool {
        let now = epoch_secs();
        let key = Self::key(ip);
        // Bound the bucket map: an attacker cycling a huge IPv6 allocation
        // can't grow it without limit; unseen keys beyond the cap are denied.
        if self.buckets.len() >= 1_000_000 && !self.buckets.contains_key(&key) {
            return false;
        }
        let entry = self.buckets.entry(key).or_default();
        let mut guard = entry.lock().unwrap();
        let w = &mut *guard;
        for (window, id, limit) in [
            (&mut w.sec, now, self.per_sec),
            (&mut w.min, now / 60, self.per_min),
            (&mut w.day, now / 86_400, self.per_day),
        ] {
            if window.0 != id {
                *window = (id, 0);
            }
            if window.1 >= limit {
                return false;
            }
        }
        w.sec.1 += 1;
        w.min.1 += 1;
        w.day.1 += 1;
        true
    }

    /// Drop buckets whose day window is stale.
    pub fn sweep(&self) {
        let today = epoch_secs() / 86_400;
        self.buckets
            .retain(|_, w| w.get_mut().unwrap().day.0 == today);
    }
}
