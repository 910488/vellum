//! Sign an update manifest with Ed25519. Used by release workflows.
//!
//! Usage: vellum-sign-update <manifest.json> <key-hex> <out.sig> <expected-public-key-hex>
//! The signature is the raw 64-byte Ed25519 signature over the file bytes.

fn main() {
    let mut args = std::env::args().skip(1);
    let manifest = args.next().expect("manifest path");
    let key_hex = args.next().expect("32-byte signing key as hex");
    let out = args.next().expect("signature output path");
    let expected_public = args.next().expect("expected public key as hex");
    let raw = std::fs::read(&manifest).expect("read manifest");
    let key_bytes = hex::decode(key_hex.trim()).expect("key hex");
    let key_array: [u8; 32] = key_bytes.try_into().expect("key must be 32 bytes");
    let signing = ed25519_dalek::SigningKey::from_bytes(&key_array);
    let actual_public = hex::encode(signing.verifying_key().to_bytes());
    assert_eq!(
        actual_public,
        expected_public.trim().to_ascii_lowercase(),
        "signing key does not match the public key embedded in release builds"
    );
    let sig = vellum_lib::updates::sign_raw(&raw, &signing);
    std::fs::write(&out, sig).expect("write signature");
}
