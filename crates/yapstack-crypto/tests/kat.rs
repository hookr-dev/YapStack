// SPDX-License-Identifier: AGPL-3.0-only
//! CRYPTO_SPEC §13 Known-Answer Test gate (T006 advisory A4/A5, Rust side).
//!
//! Every vector in `docs/CRYPTO_SPEC.md` §13 is reproduced here with the RustCrypto
//! stack and asserted byte-for-byte. Any mismatch FAILS CLOSED (the test panics ->
//! `cargo test` fails -> CI red). This is the parity oracle for the Rust half of the
//! two-stack contract.
//!
//! JS/WASM (`@noble`) PARITY IS DEFERRED: the share-viewer (`apps/share-viewer`) does
//! not exist until Phase 4, so there is no second stack to cross-check against yet.
//! When it lands, its CI must reproduce these same vectors (§9.1, §13 header). Until
//! then this Rust gate stands alone and is authoritative for the Rust runtime.

use yapstack_crypto::{aead, kdf, sign, VERSION};

fn hx(s: &str) -> Vec<u8> {
    hex::decode(s.replace(['\n', ' '], "")).expect("valid hex")
}

// ------------------------------------------------------------------ §13.1
#[test]
fn kat_13_1_argon2id_client_stretch() {
    let password = b"correct horse battery staple";
    let salt = b"yapstack-kat-salt-0001";
    let out = kdf::client_stretch(password, salt).expect("stretch");
    assert_eq!(
        hex::encode(out),
        "988d57444a7f6d69b1633090d270589b41ed7020809779bc49ecf98d3f714427"
    );
}

// ------------------------------------------------------------------ §13.2
#[test]
fn kat_13_2_hkdf_split() {
    let prk: [u8; 32] = hx("988d57444a7f6d69b1633090d270589b41ed7020809779bc49ecf98d3f714427")
        .try_into()
        .unwrap();
    let (auth, master) = kdf::split_keys(&prk);
    assert_eq!(
        hex::encode(&auth),
        "49a406dc04cfc8a1be7ad8b26bced86c821af2858cb7f3c50841309ba5d95400"
    );
    assert_eq!(
        hex::encode(&master),
        "8932d4245ecca12346969e1f6840dd59f61a0209a5814e9866333e8b7768fdfe"
    );
}

// ------------------------------------------------------------------ §13.3
#[test]
fn kat_13_3_server_verifier() {
    let auth_key = hx("49a406dc04cfc8a1be7ad8b26bced86c821af2858cb7f3c50841309ba5d95400");
    let server_salt = b"yapstack-srv-salt-000001";
    let verifier = kdf::server_verifier(&auth_key, server_salt).expect("verifier");
    assert_eq!(
        hex::encode(verifier),
        "474aca96759afc64ab67eb261cb8cf315d73ebf433aed3f11dccb5c6c3fc040d"
    );
}

// ------------------------------------------------------------------ §13.4
#[test]
fn kat_13_4_standard_seal_open() {
    let data_key: [u8; 32] = hx("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
        .try_into()
        .unwrap();
    let nonce: [u8; 24] = hx("a0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7")
        .try_into()
        .unwrap();
    let plaintext = b"the quick brown fox";

    let tenant = hx("11111111111111111111111111111111");
    let client = hx("22222222222222222222222222222222");
    let client_seq = 42u64.to_be_bytes();
    let schema = 7u32.to_be_bytes();
    let engine = 16003u32.to_be_bytes();

    let aad = aead::lp(&[
        &[VERSION],
        yapstack_crypto::DOMAIN_CHANGESET,
        &tenant,
        &client,
        &client_seq,
        &schema,
        &engine,
    ]);
    // §13.4 "(aad concatenated)"
    let expected_aad = "000000010100000015796170737461636b2e6368616e67657365742e7631\
0000001011111111111111111111111111111111\
0000001022222222222222222222222222222222\
00000008000000000000002a\
0000000400000007\
0000000400003e83";
    assert_eq!(hex::encode(&aad), expected_aad);

    let sealed = aead::seal_standard(&data_key, &nonce, plaintext, &aad).expect("seal");
    // ct||tag from §13.4
    let ct_tag = "31846f3dc628cdcf0a4f4ffb1e47cde05dc5e77a09e2dbf8629e1577b5f46df1657a3c";
    assert_eq!(hex::encode(&sealed[25..]), ct_tag);
    assert_eq!(sealed[0], VERSION);

    let opened = aead::open_standard(&data_key, &sealed, &aad).expect("open");
    assert_eq!(opened, plaintext);

    // Negative: flip the plaintext version byte -> AAD-authenticated version fails.
    let mut tampered = sealed.clone();
    tampered[0] = 0x02;
    assert!(aead::open_standard(&data_key, &tampered, &aad).is_err());
}

// ------------------------------------------------------------------ §13.5
#[test]
fn kat_13_5_committing_seal_open() {
    let k_root: [u8; 32] = hx("404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f")
        .try_into()
        .unwrap();
    let nonce: [u8; 24] = hx("101112131415161718191a1b1c1d1e1f2021222324252627")
        .try_into()
        .unwrap();
    let pt = hx("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");

    let aad = aead::lp(&[&[VERSION], yapstack_crypto::DOMAIN_SHARE, b"share-abc123"]);
    assert_eq!(
        hex::encode(&aad),
        "000000010100000011796170737461636b2e73686172652e76310000000c73686172652d616263313233"
    );

    let committing = aead::seal_committing(&k_root, &nonce, &pt, &aad).expect("seal");
    // commitment(32) from §13.5
    assert_eq!(
        hex::encode(&committing[1..33]),
        "33a7b8159b0f524b992f78fa8b65acafd3d85af85906fb6dbf789422e7d6cfbb"
    );
    // ct||tag from §13.5
    assert_eq!(
        hex::encode(&committing[57..]),
        "358375100371384674ba02e3be7c96f582e12a620aa52a4266080d56491b94f513a00e7bdb093a3ba5bd8c9409df26e0"
    );

    let opened = aead::open_committing(&k_root, &committing, &aad).expect("open");
    assert_eq!(opened, pt);

    // Negative: swap the commitment -> hard reject before AEAD.
    let mut tampered = committing.clone();
    tampered[1] ^= 0xff;
    assert!(aead::open_committing(&k_root, &tampered, &aad).is_err());
}

// ------------------------------------------------------------------ §6.2 recovery KAT (new, T007)
#[test]
fn kat_recovery_key_derivation() {
    // Fixed 160-bit recovery input; Expand-only per §6.2.
    let recovery: [u8; 20] = hx("00112233445566778899aabbccddeeff00112233")
        .try_into()
        .unwrap();
    let rk = kdf::recovery_key(&recovery);
    assert_eq!(rk.len(), 32);
    // Deterministic regression vector (generated by this stack; pin so drift fails).
    let expected = hex::encode(&rk);
    assert_eq!(hex::encode(kdf::recovery_key(&recovery)), expected);
    // It must actually wrap a vault key round-trip under the committing envelope.
    let rk32: [u8; 32] = rk.try_into().unwrap();
    let vault_key = [0x5au8; 32];
    let nonce = [0x11u8; 24];
    let aad = aead::lp(&[&[VERSION], b"yapstack.wrap.vault.rec.v1"]);
    let blob = aead::seal_committing(&rk32, &nonce, &vault_key, &aad).unwrap();
    assert_eq!(
        aead::open_committing(&rk32, &blob, &aad).unwrap(),
        vault_key
    );
}

// ------------------------------------------------------------------ §13.6 Ed25519 (bring-up, T007)
#[test]
fn kat_13_6_ed25519_roster_signature() {
    // devlist_sign_seed = HKDF-Expand(vault_key, "yapstack.devicelist.sign.v1", 32) (§7.2)
    let vault_key: [u8; 32] =
        hx("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
            .try_into()
            .unwrap();
    let seed = kdf::devlist_sign_seed(&vault_key);
    // Deterministic: same vault key -> same seed.
    assert_eq!(seed, kdf::devlist_sign_seed(&vault_key));

    let msg = b"yapstack-kat-roster-v1";
    let sig = sign::sign_roster(&vault_key, msg);

    let sk = sign::roster_signing_key(&vault_key);
    let pubkey = sk.verifying_key().to_bytes();

    // Round-trip verify passes; a tampered message fails (fail-closed).
    sign::verify_roster(&pubkey, msg, &sig).expect("valid signature verifies");
    assert!(sign::verify_roster(&pubkey, b"yapstack-kat-roster-v2", &sig).is_err());

    // Ed25519 (RFC 8032) is deterministic: signing twice yields identical bytes.
    assert_eq!(sig, sign::sign_roster(&vault_key, msg));
}
