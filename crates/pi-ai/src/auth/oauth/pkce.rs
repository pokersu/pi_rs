//! Rust 翻译自 packages/ai/src/auth/oauth/pkce.ts
//!
//! PKCE 工具。TS 使用 Web Crypto API；Rust 使用 `sha2` + `base64`。

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// 对应 `base64urlEncode(bytes)`：将字节编码为 base64url 字符串。
fn base64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 对应 `generatePKCE()`：生成 PKCE code verifier 与 challenge。
pub async fn generate_pkce() -> (String, String) {
    let mut verifier_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut verifier_bytes);
    let verifier = base64url_encode(&verifier_bytes);

    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = base64url_encode(&hasher.finalize());

    (verifier, challenge)
}
