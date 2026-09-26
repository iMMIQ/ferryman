//! Shared agent pairing protocol. The key is SHA256(token), not the raw token.
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn pairing_mac(token: &str, nonce: &str) -> Hmac<Sha256> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(&hash_token(token)).expect("HMAC accepts a SHA256 key");
    mac.update(nonce.as_bytes());
    mac
}

pub fn hmac_token(token: &str, nonce: &str) -> String {
    hex::encode(pairing_mac(token, nonce).finalize().into_bytes())
}

pub fn verify_pairing_proof(token: &str, nonce: &str, proof: &str) -> bool {
    let mut bytes = [0; 32];
    hex::decode_to_slice(proof, &mut bytes).is_ok()
        && pairing_mac(token, nonce).verify_slice(&bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pairing_protocol_is_compatible_and_rejects_invalid_proofs() {
        // Independently generated with Python hmac.new(sha256(token).digest(), nonce, sha256).
        let proof = "c3da2e31704714d0113f35df9baa8bb8910672664e08f6a34b5e5d43ea1f97c1";
        assert_eq!(hmac_token("0123456789abcdef", "pair-nonce"), proof);
        assert!(verify_pairing_proof(
            "0123456789abcdef",
            "pair-nonce",
            proof
        ));
        assert!(!verify_pairing_proof("0123456789abcdef", "other", proof));
        assert!(!verify_pairing_proof("wrong-token", "pair-nonce", proof));
        assert!(!verify_pairing_proof(
            "0123456789abcdef",
            "pair-nonce",
            &"中".repeat(22)
        ));
        assert!(!verify_pairing_proof(
            "0123456789abcdef",
            "pair-nonce",
            "00"
        ));
    }
}
