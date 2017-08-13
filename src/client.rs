//! SRP client implementation.
//! 
//! # Usage
//! First create SRP client struct by passing to it SRP parameters (shared
//! between client and server) and RNG instance (OS RNG is recommended):
//! 
//! ```ignore
//! let srp_params = SrpParams{n, g, k};
//! let mut rng = rand::os::OsRng::new().unwrap();
//! let client = SrpClient::<Sha256>::new(&srp_params, &mut rng);
//! ```
//! 
//! Next send handshake data (username and `a_pub`) to the server and receive
//! `salt` and `b_pub`:
//! 
//! ```ignore
//! let a_pub = client.get_a_pub();
//! let (salt, b_pub) = conn.send_handshake(username, a_pub);
//! ```
//! 
//! Compute private key using `salt` with any password hashing function.
//! You can use method from SRP-6a, but it's recommended to use specialized
//! password hashing algorithm instead (e.g. PBKDF2, argon2 or scrypt).
//! Next create verifier instance, note that `get_verifier` consumes client and
//! can return error in case of malicious `b_pub`.
//! 
//! ```ignore
//! let private_key = srp6a_private_key::<Sha256>(username, password, salt);
//! let verifier = client.get_verifier(&private_key, &b_pub)?;
//! ```
//!
//! Finally verify the server: first generate user proof,
//! send it to the server and verify server proof in the reply. Note that
//! `verify_server` method will return error in case of incorrect server reply.
//! 
//! ```ignore
//! let user_proof = verifier.get_proof();
//! let server_proof = conn.send_proof(user_proof);
//! let key = verifier.verify_server(server_proof)?;
//! ```
//! 
//! `key` contains shared secret key between user and the server. Alternatively
//! you can directly extract shared secret key using `get_key()` method and
//! handle authentification through different (secure!) means (e.g. by using
//! authentificated cipher mode).
//! 
//! For user registration on the server first generate salt (e.g. 32 bytes long)
//! and get password verifier which depends on private key. Send useranme, salt
//! and password verifier over protected channel to protect against MitM for
//! registration.
//! 
//! ```ignore
//! let pwd_verifier = SrpClient::<Sha256>::register(&private_key, &srp_params);
//! conn.send_registration_data(username, salt, pwd_verifier);
//! ```
use std::marker::PhantomData;

use rand::Rng;
use num::{BigUint, Zero};
use digest::Digest;
use generic_array::GenericArray;

use tools::powm;
use types::{SrpAuthError, SrpParams};

/// SRP client state before handshake with the server.
pub struct SrpClient<'a, D: Digest> {
    params: &'a SrpParams,

    a: BigUint,
    a_pub: BigUint,

    d: PhantomData<D>
}

/// SRP client state after handshake with the server.
pub struct SrpClientVerifier<D: Digest> {
    proof: GenericArray<u8, D::OutputSize>,
    server_proof: GenericArray<u8, D::OutputSize>,
    key: GenericArray<u8, D::OutputSize>,
}

/// Compute user private key as described in the SRP6a. Consider using proper
/// password hashing algorithm instead.
pub fn srp6a_private_key<D: Digest>(username: &[u8], password: &[u8],
        salt: &[u8]
    ) -> GenericArray<u8, D::OutputSize>
{
    let p = {
        let mut d = D::new();
        d.input(username);
        d.input(b":");
        d.input(password);
        d.result()
    };
    let mut d = D::new();
    d.input(salt);
    d.input(&p);
    d.result()
}

impl<'a, D: Digest> SrpClient<'a, D> {
    /// Create new SRP client instance.
    pub fn new<R: Rng>(params: &'a SrpParams, rng: &mut R) -> Self {
        let l = params.n.bits()/8; 
        let buf = rng.gen_iter::<u8>().take(l).collect::<Vec<u8>>();
        let a = BigUint::from_bytes_le(&buf);
        let a_pub = params.powm(&a);

        Self { params, a, a_pub, d: Default::default() }
    }

    /// Get password verfier for user registration on the server
    pub fn get_password_verifier(&self, private_key: &[u8]) -> Vec<u8> {
        let x = BigUint::from_bytes_le(&private_key);
        let v = self.params.powm(&x);
        v.to_bytes_le()
    }

    fn calc_key(&self, b_pub: &BigUint, x: &BigUint, u: &BigUint)
        -> GenericArray<u8, D::OutputSize>
    {
        let n = &self.params.n;
        let interm = (&self.params.k * self.params.powm(x)) % n;
        // Because we do operation in modulo N we can get: (kv + g^b) < kv
        let v = if b_pub > &interm {
            (b_pub - &interm) % n
        } else {
            (n + b_pub - &interm) % n
        };
        // S = |B - kg^x| ^ (a + ux)
        let s = powm(&v, &(&self.a + (u*x) % n ), n);
        D::digest(&s.to_bytes_le())
    }

    /// Process server reply to the handshake.
    pub fn process_reply(self, private_key: &[u8], b_pub: &[u8])
        -> Result<SrpClientVerifier<D>, SrpAuthError>
    {
        let u = {
            let mut d = D::new();
            d.input(&self.a_pub.to_bytes_le());
            d.input(b_pub);
            BigUint::from_bytes_le(&d.result())
        };

        let b_pub = BigUint::from_bytes_le(b_pub);

        // Safeguard against malicious B
        if &b_pub % &self.params.n == BigUint::zero() {
            return Err(SrpAuthError{ description: "Malicious b_pub value" })
        }

        let x = BigUint::from_bytes_le(&private_key);
        let key = self.calc_key(&b_pub, &x, &u);
        // M1 = H(A, B, K)
        let proof = {
            let mut d = D::new();
            d.input(&self.a_pub.to_bytes_le());
            d.input(&b_pub.to_bytes_le());
            d.input(&key);
            d.result()
        };

        // M2 = H(A, M1, K)
        let server_proof = {
            let mut d = D::new();
            d.input(&self.a_pub.to_bytes_le());
            d.input(&proof);
            d.input(&key);
            d.result()
        };

        Ok(SrpClientVerifier {
            proof: proof,
            server_proof: server_proof,
            key: key,
        })
    }

    /// Get public ephemeral value for handshaking with the server.
    pub fn get_a_pub(&self) -> Vec<u8> {
        self.a_pub.to_bytes_le()
    }
}

impl<D: Digest> SrpClientVerifier<D> {
    /// Get shared secret key without authenticating server, e.g. for using with
    /// authenticated encryption modes. DO NOT USE this method without
    /// some kind of secure authentification.
    pub fn get_key(self) -> GenericArray<u8, D::OutputSize> {
        self.key
    }

    /// Verification data for sending to the server.
    pub fn get_proof(&self) -> GenericArray<u8, D::OutputSize> {
        self.proof.clone()
    }

    /// Verify server reply to verification data. It will return shared secret
    /// key in case of success.
    pub fn verify_server(self, reply: &[u8])
        -> Result<GenericArray<u8, D::OutputSize>, SrpAuthError>
    {
        if self.server_proof.as_slice() != reply {
            Err(SrpAuthError{ description: "Incorrect server proof" })
        } else {
            Ok(self.key)
        }
    }
}