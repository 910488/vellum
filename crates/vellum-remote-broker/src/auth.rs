//! Device authentication helpers.

use sha2::{Digest, Sha256};

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn tokens_match(token: &str, token_hash: &str) -> bool {
    hash_token(token) == token_hash
}
