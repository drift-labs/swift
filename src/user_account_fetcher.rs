use anchor_lang::AccountDeserialize;
use base64::Engine;
use deadpool_redis::Pool;
use drift_rs::{types::accounts::User, DriftClient};
use redis::AsyncCommands;
use solana_sdk::{clock::Slot, pubkey::Pubkey};

#[derive(Clone)]
/// Provides drift User account fetching
pub enum UserAccountFetcherImpl {
    /// fetch by UserMap
    UserMap(UserMapFetcher),
    Rpc(DriftClient),
}

impl UserAccountFetcher for UserAccountFetcherImpl {
    async fn get_user(&self, account: &Pubkey, slot: Slot) -> Result<User, ()> {
        match self {
            Self::UserMap(u) => u.get_user(account, slot).await,
            Self::Rpc(r) => r.get_user_account(account).await.map_err(|err| {
                log::warn!("failed to fetch user: {account}. {err:?}");
            }),
        }
    }
}

/// Provides drift User account fetching
pub trait UserAccountFetcher {
    /// Fetch the `User` account data
    ///
    /// - `account` pubkey of the user subaccount
    /// - `slot` current slot
    async fn get_user(&self, account: &Pubkey, slot: Slot) -> Result<User, ()>;
}

#[derive(Clone)]
/// Fetches users from UserMap server
pub struct UserMapFetcher {
    redis: Pool,
}

impl UserMapFetcher {
    pub fn from_env() -> Self {
        let redis = {
            let elasticache_host = std::env::var("USERMAP_ELASTICACHE_HOST")
                .unwrap_or_else(|_| "localhost".to_string());
            let elasticache_port =
                std::env::var("USERMAP_ELASTICACHE_PORT").unwrap_or_else(|_| "6379".to_string());
            let connection_string = if std::env::var("USERMAP_USE_SSL")
                .unwrap_or_else(|_| "false".to_string())
                .to_lowercase()
                == "true"
            {
                format!("rediss://{}:{}", elasticache_host, elasticache_port)
            } else {
                format!("redis://{}:{}", elasticache_host, elasticache_port)
            };
            let cfg = deadpool_redis::Config::from_url(connection_string);
            cfg.create_pool(Some(deadpool_redis::Runtime::Tokio1))
                .expect("Failed to create Redis pool")
        };

        Self { redis }
    }
}

impl UserAccountFetcher for UserMapFetcher {
    async fn get_user(&self, account: &Pubkey, slot: Slot) -> Result<User, ()> {
        let mut conn = self.redis.get().await.map_err(|err| {
            log::error!("usermap redis pool: {err:?}");
        })?;

        let redis_key = format!("usermap-server::{}", account);

        match conn.get::<_, Option<String>>(&redis_key).await {
            Ok(Some(value)) => {
                let value_vec: Vec<&str> = value.split("::").collect();
                let redis_slot = value_vec[0].parse::<u64>().unwrap();

                if slot.saturating_sub(redis_slot) > (90_f64 * 2.5) as u64 {
                    log::warn!("User found in redis is too old");
                    return Err(());
                }

                let user = User::try_deserialize(
                    &mut &base64::engine::general_purpose::STANDARD
                        .decode(value_vec[1])
                        .unwrap()[..],
                );

                user.map_err(|err| {
                    log::error!("Failed to deserialize user from redis: {err:?}");
                })
            }
            Ok(None) => {
                log::warn!("No value found for usermap key: {}", &redis_key);
                Err(())
            }
            Err(err) => {
                log::error!("usermap query error: {err:?}");
                Err(())
            }
        }
    }
}
