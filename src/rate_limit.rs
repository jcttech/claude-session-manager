use axum::{
    body::Body,
    http::{Request, Response, StatusCode},
};
use dashmap::DashMap;
use std::{
    future::Future,
    net::IpAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tower::{Layer, Service};

/// Simple token bucket rate limiter
#[derive(Clone)]
pub struct RateLimitLayer {
    state: Arc<RateLimitState>,
}

struct RateLimitState {
    buckets: DashMap<IpAddr, TokenBucket>,
    requests_per_second: u64,
    burst_size: u32,
}

struct TokenBucket {
    tokens: f64,
    last_update: Instant,
}

impl RateLimitLayer {
    pub fn new(requests_per_second: u64, burst_size: u32) -> Self {
        Self {
            state: Arc::new(RateLimitState {
                buckets: DashMap::new(),
                requests_per_second,
                burst_size,
            }),
        }
    }
}

impl<S> Layer<S> for RateLimitLayer {
    type Service = RateLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimitService {
            inner,
            state: self.state.clone(),
        }
    }
}

#[derive(Clone)]
pub struct RateLimitService<S> {
    inner: S,
    state: Arc<RateLimitState>,
}

impl<S> Service<Request<Body>> for RateLimitService<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // Extract client IP from headers or connection info
        let client_ip = extract_client_ip(&req);

        let allowed = if let Some(ip) = client_ip {
            self.check_rate_limit(ip)
        } else {
            // If we can't determine IP, allow the request but log it
            tracing::warn!("Could not determine client IP for rate limiting");
            true
        };

        if !allowed {
            let response = Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .header("Retry-After", "1")
                .body(Body::from("Rate limit exceeded"))
                .unwrap();

            return Box::pin(async move { Ok(response) });
        }

        let future = self.inner.call(req);
        Box::pin(async move { future.await })
    }
}

impl<S> RateLimitService<S> {
    fn check_rate_limit(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let rps = self.state.requests_per_second as f64;
        let burst = self.state.burst_size as f64;

        let mut bucket = self.state.buckets.entry(ip).or_insert_with(|| TokenBucket {
            tokens: burst,
            last_update: now,
        });

        // Add tokens based on time elapsed
        let elapsed = now.duration_since(bucket.last_update);
        let new_tokens = elapsed.as_secs_f64() * rps;
        bucket.tokens = (bucket.tokens + new_tokens).min(burst);
        bucket.last_update = now;

        // Check if we have tokens available
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

fn extract_client_ip(req: &Request<Body>) -> Option<IpAddr> {
    // Try X-Forwarded-For header first (for reverse proxy setups)
    if let Some(forwarded) = req.headers().get("x-forwarded-for") {
        if let Ok(value) = forwarded.to_str() {
            // Take the first IP in the chain (original client)
            if let Some(first_ip) = value.split(',').next() {
                if let Ok(ip) = first_ip.trim().parse::<IpAddr>() {
                    return Some(ip);
                }
            }
        }
    }

    // Try X-Real-IP header
    if let Some(real_ip) = req.headers().get("x-real-ip") {
        if let Ok(value) = real_ip.to_str() {
            if let Ok(ip) = value.trim().parse::<IpAddr>() {
                return Some(ip);
            }
        }
    }

    // Fall back to connection info (would need to be passed differently in production)
    // For now, return None if headers aren't present
    None
}

/// Periodically clean up old rate limit buckets to prevent memory leaks
pub fn spawn_cleanup_task(layer: RateLimitLayer) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(300)); // Every 5 minutes
        loop {
            interval.tick().await;
            let now = Instant::now();
            let stale_threshold = Duration::from_secs(600); // 10 minutes

            layer.state.buckets.retain(|_, bucket| {
                now.duration_since(bucket.last_update) < stale_threshold
            });

            tracing::debug!("Rate limit cleanup: {} active buckets", layer.state.buckets.len());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn make_test_service() -> RateLimitService<()> {
        let layer = RateLimitLayer::new(10, 5); // 10 rps, burst of 5
        RateLimitService {
            inner: (),
            state: layer.state,
        }
    }

    #[test]
    fn test_rate_limit_allows_burst() {
        let service = make_test_service();
        let ip: IpAddr = Ipv4Addr::new(192, 168, 1, 1).into();

        // Should allow burst_size requests immediately
        for i in 0..5 {
            assert!(service.check_rate_limit(ip), "Request {} should be allowed", i);
        }
    }

    #[test]
    fn test_rate_limit_blocks_after_burst() {
        let service = make_test_service();
        let ip: IpAddr = Ipv4Addr::new(192, 168, 1, 2).into();

        // Exhaust the burst
        for _ in 0..5 {
            service.check_rate_limit(ip);
        }

        // Next request should be blocked
        assert!(!service.check_rate_limit(ip), "Request after burst should be blocked");
    }

    #[test]
    fn test_rate_limit_different_ips_independent() {
        let service = make_test_service();
        let ip1: IpAddr = Ipv4Addr::new(192, 168, 1, 1).into();
        let ip2: IpAddr = Ipv4Addr::new(192, 168, 1, 2).into();

        // Exhaust burst for ip1
        for _ in 0..5 {
            service.check_rate_limit(ip1);
        }

        // ip2 should still have full burst
        for i in 0..5 {
            assert!(service.check_rate_limit(ip2), "IP2 request {} should be allowed", i);
        }

        // ip1 should still be blocked
        assert!(!service.check_rate_limit(ip1));
    }

    #[test]
    fn test_rate_limit_tokens_refill() {
        let layer = RateLimitLayer::new(1000, 1); // 1000 rps, burst of 1
        let service = RateLimitService {
            inner: (),
            state: layer.state,
        };
        let ip: IpAddr = Ipv4Addr::new(192, 168, 1, 1).into();

        // Use the one token
        assert!(service.check_rate_limit(ip));
        assert!(!service.check_rate_limit(ip));

        // Wait a bit for token refill (1000 rps = 1 token per ms)
        std::thread::sleep(Duration::from_millis(2));

        // Should have refilled
        assert!(service.check_rate_limit(ip));
    }

    #[test]
    fn test_extract_client_ip_from_x_forwarded_for() {
        let req = Request::builder()
            .header("x-forwarded-for", "203.0.113.195, 70.41.3.18, 150.172.238.178")
            .body(Body::empty())
            .unwrap();

        let ip = extract_client_ip(&req);
        assert_eq!(ip, Some(Ipv4Addr::new(203, 0, 113, 195).into()));
    }

    #[test]
    fn test_extract_client_ip_from_x_real_ip() {
        let req = Request::builder()
            .header("x-real-ip", "203.0.113.195")
            .body(Body::empty())
            .unwrap();

        let ip = extract_client_ip(&req);
        assert_eq!(ip, Some(Ipv4Addr::new(203, 0, 113, 195).into()));
    }

    #[test]
    fn test_extract_client_ip_prefers_x_forwarded_for() {
        let req = Request::builder()
            .header("x-forwarded-for", "1.1.1.1")
            .header("x-real-ip", "2.2.2.2")
            .body(Body::empty())
            .unwrap();

        let ip = extract_client_ip(&req);
        assert_eq!(ip, Some(Ipv4Addr::new(1, 1, 1, 1).into()));
    }

    #[test]
    fn test_extract_client_ip_none_without_headers() {
        let req = Request::builder()
            .body(Body::empty())
            .unwrap();

        let ip = extract_client_ip(&req);
        assert_eq!(ip, None);
    }

    #[test]
    fn test_extract_client_ip_invalid_header() {
        let req = Request::builder()
            .header("x-forwarded-for", "not-an-ip")
            .body(Body::empty())
            .unwrap();

        let ip = extract_client_ip(&req);
        assert_eq!(ip, None);
    }

    #[test]
    fn test_extract_client_ip_ipv6() {
        let req = Request::builder()
            .header("x-real-ip", "2001:db8::1")
            .body(Body::empty())
            .unwrap();

        let ip = extract_client_ip(&req);
        assert!(ip.is_some());
        assert!(ip.unwrap().is_ipv6());
    }

    #[test]
    fn test_rate_limit_layer_cloneable() {
        let layer = RateLimitLayer::new(10, 5);
        let _clone = layer.clone();
    }

    #[test]
    fn test_bucket_count() {
        let layer = RateLimitLayer::new(10, 5);
        let service = RateLimitService {
            inner: (),
            state: layer.state.clone(),
        };

        assert_eq!(layer.state.buckets.len(), 0);

        service.check_rate_limit(Ipv4Addr::new(1, 1, 1, 1).into());
        assert_eq!(layer.state.buckets.len(), 1);

        service.check_rate_limit(Ipv4Addr::new(2, 2, 2, 2).into());
        assert_eq!(layer.state.buckets.len(), 2);

        // Same IP doesn't add new bucket
        service.check_rate_limit(Ipv4Addr::new(1, 1, 1, 1).into());
        assert_eq!(layer.state.buckets.len(), 2);
    }
}
