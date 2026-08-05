use crate::error::{NexusError, Result};
use crate::privacy::crypto::{decrypt, encrypt, Nonce, SymmetricKey};

const TAG_LEN: usize = 16;
const NONCE_LEN: usize = 12;

pub struct EncryptedPacket {
    pub nonce: Nonce,
    pub ciphertext: Vec<u8>,
}

impl EncryptedPacket {
    pub fn new(key: &SymmetricKey, plaintext: &[u8]) -> Result<Self> {
        let (nonce, ciphertext) = encrypt(key, plaintext)?;
        Ok(EncryptedPacket { nonce, ciphertext })
    }

    pub fn decrypt(&self, key: &SymmetricKey) -> Result<Vec<u8>> {
        decrypt(key, &self.nonce, &self.ciphertext)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(NONCE_LEN + self.ciphertext.len());
        bytes.extend_from_slice(&self.nonce);
        bytes.extend_from_slice(&self.ciphertext);
        bytes
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < NONCE_LEN + TAG_LEN {
            return Err(NexusError::Other(
                "data too short to contain a valid encrypted packet".to_string(),
            ));
        }

        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&data[..NONCE_LEN]);
        let ciphertext = data[NONCE_LEN..].to_vec();

        Ok(EncryptedPacket { nonce, ciphertext })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privacy::crypto::derive_key;

    #[test]
    fn wire_round_trip() {
        let key = derive_key(b"transport test key material", b"salt");
        let packet = EncryptedPacket::new(&key, b"payload over the wire").unwrap();

        let bytes = packet.to_bytes();
        let reconstructed = EncryptedPacket::from_bytes(&bytes).unwrap();
        let plaintext = reconstructed.decrypt(&key).unwrap();

        assert_eq!(plaintext, b"payload over the wire");
    }

    #[test]
    fn from_bytes_rejects_data_too_short_to_be_valid() {
        let too_short = vec![0u8; NONCE_LEN + TAG_LEN - 1];
        assert!(EncryptedPacket::from_bytes(&too_short).is_err());
    }

    #[test]
    fn decrypt_with_wrong_key_fails() {
        let key_a = derive_key(b"key a", b"salt");
        let key_b = derive_key(b"key b", b"salt");
        let packet = EncryptedPacket::new(&key_a, b"secret").unwrap();
        assert!(packet.decrypt(&key_b).is_err());
    }
}
