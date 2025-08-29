// --- ANNOTATED IMPLEMENTATION ---
// This implementation is based on the official Engine API specification.
// It generates JWTs with an `iat` claim but no `exp` claim.
// The token is cached and regenerated periodically to ensure the `iat`
// timestamp is always recent enough to be accepted by the execution client.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use color_eyre::eyre::Ok;
use jsonwebtoken::{Header, encode};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::ExecutionError;

// NOTE: The token's effective lifetime is determined by the execution client's
// validation of the `iat` (issued-at) claim. The spec recommends ELs
// reject tokens with an `iat` older than 60s. We use a shorter lifetime
// here to ensure our token is always considered "fresh".
const TOKEN_VALIDITY_DURATION: Duration = Duration::from_secs(55);

#[derive(Debug, Serialize, Deserialize)]
/// Claims for the JWT token, as required by the Engine API specification.
struct Claims {
    iat: u64,
}

/// Caches a JWT token and its creation time.
#[derive(Clone)]
struct JwtCache {
    token: String,
    created_at: SystemTime,
}

/// Provides JWT tokens for authenticating with the Engine API.
pub struct JwtProvider {
    key: jsonwebtoken::EncodingKey,
    cache: RwLock<Option<JwtCache>>,
}

impl JwtProvider {
    /// Creates a new `JwtProvider` with the give secret.
    pub fn new(secret: [u8; 32]) -> Self {
        Self { key: jsonwebtoken::EncodingKey::from_secret(&secret), cache: RwLock::new(None) }
    }

    /// Returns a valid JWT token, either from the cache or by generating a new one.
    pub async fn get_token(&self) -> color_eyre::eyre::Result<String> {
        // First, check for a valid token using a read lock for concurrency
        {
            let cached_guard = self.cache.read().await;
            if let Some(cached) = cached_guard.as_ref() {
                if cached.created_at.elapsed()? < TOKEN_VALIDITY_DURATION {
                    return Ok(cached.token.clone())
                }
            }
        }

        // If no valid token exists acquire a write lock to generate one.

        let mut cache = self.cache.write().await;

        if let Some(cached) = cache.as_ref() {
            if cached.created_at.elapsed()? < TOKEN_VALIDITY_DURATION {
                return Ok(cached.token.clone())
            }
        }

        // Generate a new token.
        //
        let now = SystemTime::now();
        let iat = now.duration_since(UNIX_EPOCH)?.as_secs();
        let claims = Claims { iat };
        let token = encode(&Header::default(), &claims, &self.key)
            .map_err(|e| ExecutionError::Jwt(e.to_string()))?;

        cache.replace(JwtCache { token: token.clone(), created_at: now });

        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use std::thread::sleep;

    use sha3::digest::typenum::assert_type;

    use super::*;

    #[tokio::test]
    async fn can_generate_and_cache_token() {
        let secret = [1; 32];
        let provider = JwtProvider::new(secret);
        let token1 = provider.get_token().await.unwrap();
        assert!(!token1.is_empty());

        let token2 = provider.get_token().await.unwrap();
        assert_eq!(token1, token2);
    }

    #[tokio::test]
    async fn test_concurrent_token_regeneration_with_joinset() {
        use std::{sync::Arc, time::Duration};

        use tokio::task::JoinSet; // Import JoinSet

        let secret = [4u8; 32];
        let provider = Arc::new(JwtProvider::new(secret));

        // 1. Get an initial token to populate the cache.
        let initial_token = provider.get_token().await.unwrap();

        // 2. Manually invalidate the cache.
        {
            let mut cache = provider.cache.write().await;
            if let Some(cached) = cache.as_mut() {
                cached.created_at = std::time::SystemTime::now() - Duration::from_secs(60);
            }
        }

        // Ensure the clock ticks over to the next second
        tokio::time::sleep(Duration::from_secs(1)).await;

        // 3. Spawn multiple concurrent tasks using a JoinSet.
        let mut set = JoinSet::new();
        for _ in 0..10 {
            let provider_clone = provider.clone();
            // spawn returns a handle, but we don't need it as the set manages it.
            set.spawn(async move { provider_clone.get_token().await.unwrap() });
        }

        // 4. Collect the results from the set.
        let mut new_tokens = Vec::new();
        while let Some(res) = set.join_next().await {
            new_tokens.push(res.unwrap());
        }

        // Sanity check that we got all results back
        assert_eq!(new_tokens.len(), 10);

        // 5. Verify all new tokens are identical to each other.
        let first_new_token = &new_tokens[0];
        assert!(new_tokens.iter().all(|t| t == first_new_token));

        // 6. Verify that the regenerated token is different from the original expired one.
        assert_ne!(&initial_token, first_new_token);
    }

    #[tokio::test]
    async fn test_cache_expiration() {
        let secret = [3u8; 32];
        let provider = JwtProvider::new(secret);

        // Generate initial token
        let token1 = provider.get_token().await.unwrap();

        // Manually invalidate cache by setting old timestamp
        {
            let mut cache = provider.cache.write().await;
            if let Some(cached) = cache.as_mut() {
                cached.created_at = SystemTime::now() - Duration::from_secs(60);
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Next call should generate a new token
        let token2 = provider.get_token().await.unwrap();
        assert_ne!(token1, token2);
    }
}
