use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::privacy::crypto::{
    decrypt, derive_key, encrypt, generate_keypair, key_agreement, PrivateKey, PublicKey,
    SymmetricKey,
};

pub struct Session {
    pub id: [u8; 32],
    pub created_at: Instant,
    pub key: SymmetricKey,
}

pub struct SessionManager {
    sessions: Mutex<HashMap<[u8; 32], Session>>,
    session_ttl: Duration,
    server_private: PrivateKey,
    server_public: PublicKey,
}

impl SessionManager {
    pub fn new(ttl: Duration) -> Self {
        let (server_public, server_private) = generate_keypair();
        SessionManager {
            sessions: Mutex::new(HashMap::new()),
            session_ttl: ttl,
            server_private,
            server_public,
        }
    }

    pub fn create_session(&self) -> (Session, Vec<u8>) {
        let mut id = [0u8; 32];
        rand::Rng::fill(&mut rand::thread_rng(), &mut id);

        let session_key = derive_key(&id, b"nexus-session");
        let session = Session {
            id,
            created_at: Instant::now(),
            key: session_key,
        };

        let mut data = Vec::with_capacity(64);
        data.extend_from_slice(&id);
        data.extend_from_slice(&self.server_public.0);

        self.sessions.lock().unwrap().insert(
            id,
            Session {
                id,
                created_at: Instant::now(),
                key: SymmetricKey(session.key.0),
            },
        );

        (session, data)
    }

    pub fn negotiate(
        &self,
        session_id: [u8; 32],
        peer_public: &[u8],
    ) -> std::result::Result<Session, ()> {
        if peer_public.len() != 32 {
            return Err(());
        }

        let mut pub_bytes = [0u8; 32];
        pub_bytes.copy_from_slice(peer_public);
        let peer_key = PublicKey(pub_bytes);

        let shared = key_agreement(&self.server_private, &peer_key);
        let session_key = derive_key(&shared.0, &session_id);

        let session = Session {
            id: session_id,
            created_at: Instant::now(),
            key: session_key,
        };

        self.sessions.lock().unwrap().insert(
            session_id,
            Session {
                id: session_id,
                created_at: Instant::now(),
                key: SymmetricKey(session.key.0),
            },
        );

        Ok(session)
    }

    pub fn get(&self, id: &[u8; 32]) -> Option<Session> {
        self.sessions.lock().unwrap().get(id).map(|s| Session {
            id: s.id,
            created_at: s.created_at,
            key: SymmetricKey(s.key.0),
        })
    }

    /// Removes sessions older than the configured TTL.
    ///
    /// Deliberately compares each session's *elapsed* duration rather than
    /// computing a cutoff via `Instant::now() - self.session_ttl`: that
    /// subtraction can panic if `session_ttl` exceeds how long the process
    /// has been running (e.g. right after a restart with a multi-day TTL
    /// configured), since `Instant` has no meaningful "before process
    /// start" value on some platforms.
    pub fn expire_old(&self) {
        let ttl = self.session_ttl;
        self.sessions
            .lock()
            .unwrap()
            .retain(|_, s| s.created_at.elapsed() < ttl);
    }

    pub fn encrypt(
        &self,
        session_id: &[u8; 32],
        plaintext: &[u8],
    ) -> std::result::Result<Vec<u8>, ()> {
        let key = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .map(|s| SymmetricKey(s.key.0))
            .ok_or(())?;
        let (nonce, ciphertext) = encrypt(&key, plaintext).map_err(|_| ())?;
        let mut packet = Vec::with_capacity(12 + ciphertext.len());
        packet.extend_from_slice(&nonce);
        packet.extend_from_slice(&ciphertext);
        Ok(packet)
    }

    pub fn decrypt(&self, session_id: &[u8; 32], data: &[u8]) -> std::result::Result<Vec<u8>, ()> {
        if data.len() < 12 {
            return Err(());
        }
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&data[..12]);
        let key = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .map(|s| SymmetricKey(s.key.0))
            .ok_or(())?;
        decrypt(&key, &nonce, &data[12..]).map_err(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_session_round_trips_through_get() {
        let manager = SessionManager::new(Duration::from_secs(3600));
        let (session, handshake_data) = manager.create_session();
        assert_eq!(handshake_data.len(), 64); // session id (32) + server public key (32)

        let fetched = manager.get(&session.id).expect("session should be retrievable");
        assert_eq!(fetched.id, session.id);
        assert_eq!(fetched.key.0, session.key.0);
    }

    #[test]
    fn get_returns_none_for_unknown_session() {
        let manager = SessionManager::new(Duration::from_secs(3600));
        assert!(manager.get(&[0u8; 32]).is_none());
    }

    #[test]
    fn negotiate_full_handshake_between_client_and_server() {
        let server = SessionManager::new(Duration::from_secs(3600));
        let (client_public, client_private) = generate_keypair();

        // Server issues a session id (normally sent to the client alongside
        // its own public key via create_session; here we just need an id).
        let (server_session, handshake) = server.create_session();
        let server_public_bytes = &handshake[32..64];
        let mut server_public = [0u8; 32];
        server_public.copy_from_slice(server_public_bytes);

        // Client independently derives the same shared secret using its
        // private key and the server's public key.
        let client_shared = key_agreement(&client_private, &PublicKey(server_public));
        let client_session_key = derive_key(&client_shared.0, &server_session.id);

        // Server negotiates using the client's public key.
        let server_negotiated = server
            .negotiate(server_session.id, &client_public.0)
            .expect("negotiate should succeed with a valid 32-byte public key");

        assert_eq!(
            client_session_key.0, server_negotiated.key.0,
            "both sides must derive the same session key"
        );
    }

    #[test]
    fn negotiate_rejects_malformed_public_key() {
        let server = SessionManager::new(Duration::from_secs(3600));
        let (session, _) = server.create_session();
        let too_short = [0u8; 16];
        assert!(server.negotiate(session.id, &too_short).is_err());
    }

    #[test]
    fn encrypt_decrypt_round_trip_via_manager() {
        let manager = SessionManager::new(Duration::from_secs(3600));
        let (session, _) = manager.create_session();

        let ciphertext = manager
            .encrypt(&session.id, b"a secret message")
            .expect("encryption should succeed for a known session");
        let plaintext = manager
            .decrypt(&session.id, &ciphertext)
            .expect("decryption should succeed with the matching session");
        assert_eq!(plaintext, b"a secret message");
    }

    #[test]
    fn decrypt_fails_for_unknown_session() {
        let manager = SessionManager::new(Duration::from_secs(3600));
        let result = manager.decrypt(&[0xAAu8; 32], &[0u8; 32]);
        assert!(result.is_err());
    }

    #[test]
    fn decrypt_fails_for_tampered_ciphertext() {
        let manager = SessionManager::new(Duration::from_secs(3600));
        let (session, _) = manager.create_session();
        let mut ciphertext = manager.encrypt(&session.id, b"message").unwrap();
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0xFF;
        assert!(manager.decrypt(&session.id, &ciphertext).is_err());
    }

    #[test]
    fn expire_old_removes_sessions_past_ttl() {
        let manager = SessionManager::new(Duration::from_millis(10));
        let (session, _) = manager.create_session();
        assert!(manager.get(&session.id).is_some());

        std::thread::sleep(Duration::from_millis(30));
        manager.expire_old();
        assert!(manager.get(&session.id).is_none());
    }

    #[test]
    fn expire_old_does_not_panic_with_ttl_longer_than_process_uptime() {
        // Regression test for the Instant-subtraction-underflow bug: a
        // long TTL relative to how long the process has been running used
        // to be able to panic inside `Instant::now() - session_ttl`.
        let manager = SessionManager::new(Duration::from_secs(60 * 60 * 24 * 365));
        let (_session, _) = manager.create_session();
        manager.expire_old(); // must not panic
    }
}
