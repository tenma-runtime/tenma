//! Password hashes for PHP (Security\Password): Argon2id on tokio's background threads, so a sign-in does
//! not hold up every other request of the worker for the 100 ms or more one hash takes. The hashes are
//! the strings of PHP's password_hash() with PASSWORD_ARGON2ID and its default costs, so either side
//! verifies what the other made. Hashes of other algorithms (bcrypt) stay with PHP's password_verify().
//!
//! At most one hash per CPU runs at a time, across all workers: each takes 64 MB, and a burst of
//! sign-ins would otherwise start a thread with 64 MB for every one of them.

use std::sync::LazyLock;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::Semaphore;

use crate::ops;
use crate::php::{OpResult, bytes};

/// PHP's PASSWORD_ARGON2_DEFAULT_MEMORY_COST (KiB), _TIME_COST and _THREADS
const MEMORY: u32 = 65_536;
const TIME: u32 = 4;
const THREADS: u32 = 1;

static RUNNING: LazyLock<Semaphore> = LazyLock::new(|| {
    Semaphore::new(std::thread::available_parallelism().map_or(1, |cpus| cpus.get()))
});

fn hash(password: &[u8]) -> Result<String, String> {
    let mut salt = [0u8; 16];
    SystemRandom::new()
        .fill(&mut salt)
        .map_err(|_| "no random bytes for the salt".to_string())?;
    let salt = SaltString::encode_b64(&salt).map_err(|error| error.to_string())?;
    let params = Params::new(MEMORY, TIME, THREADS, None).map_err(|error| error.to_string())?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(password, &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| error.to_string())
}

/// The costs come from the hash itself; a hash that is not Argon2 does not match.
fn verify(password: &[u8], hash: &str) -> bool {
    PasswordHash::new(hash)
        .is_ok_and(|parsed| Argon2::default().verify_password(password, &parsed).is_ok())
}

/// Starts "hash" of $password, or "verify" of $password against the hash $stored ("1" or "0"). The password never
/// goes into the op's detail: a recording keeps only the result.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_password(
    op: *const u8,
    op_len: usize,
    password: *const u8,
    password_len: usize,
    stored: *const u8,
    stored_len: usize,
) -> u64 {
    let op = String::from_utf8_lossy(bytes(op, op_len)).into_owned();
    let password = bytes(password, password_len).to_vec();
    let stored = String::from_utf8_lossy(bytes(stored, stored_len)).into_owned();
    ops::spawn_uncancellable("password", op.clone(), async move {
        let Ok(_permit) = RUNNING.acquire().await else {
            return OpResult::Error("password: closed".into());
        };
        let job = tokio::task::spawn_blocking(move || match op.as_str() {
            "hash" => hash(&password),
            "verify" => Ok(if verify(&password, &stored) { "1" } else { "0" }.to_string()),
            _ => Err(format!("unknown op {op}")),
        });
        match job.await {
            Ok(Ok(text)) => OpResult::Text(text.into_bytes()),
            Ok(Err(error)) => OpResult::Error(format!("password: {error}")),
            Err(error) => OpResult::Error(format!("password: {error}")),
        }
    })
}
