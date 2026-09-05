//! Apple Remote Desktop (RFB security type 30).
//!
//! DH key agreement, MD5 of the fixed-width shared secret, then AES-128-ECB
//! over two randomly padded, NUL-terminated 64-byte credential fields.
//! Only the credentials are encrypted; this does not provide session TLS.

use openssl::bn::{BigNum, BigNumContext};
use openssl::hash::{MessageDigest, hash};
use openssl::rand::rand_bytes;
use openssl::symm::{Cipher, Crypter, Mode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

use crate::SessionError;

pub(crate) const SECURITY_TYPE: u8 = 30;

fn crypto_error(error: openssl::error::ErrorStack) -> SessionError {
    SessionError::msg(format!("ARD authentication error: {error}"))
}

fn validate_credentials(username: &str, password: &str) -> Result<(), SessionError> {
    if username.is_empty() || password.is_empty() {
        return Err(SessionError::msg(
            "This Mac requires a username and password. Enter the Mac login name in connection properties.",
        ));
    }
    for (label, value) in [("Username", username), ("Password", password)] {
        if value.len() > 63 || value.contains('\0') {
            return Err(SessionError::msg(format!(
                "{label} must fit in 63 UTF-8 bytes and contain no NUL characters for Mac login"
            )));
        }
    }
    Ok(())
}

fn encrypt_credentials(shared: &[u8], credentials: &[u8; 128]) -> Result<[u8; 128], SessionError> {
    let key = Zeroizing::new(
        hash(MessageDigest::md5(), shared)
            .map_err(crypto_error)?
            .to_vec(),
    );
    let mut cipher =
        Crypter::new(Cipher::aes_128_ecb(), Mode::Encrypt, &key, None).map_err(crypto_error)?;
    cipher.pad(false);
    let mut output = [0u8; 144];
    let count = cipher
        .update(credentials, &mut output)
        .map_err(crypto_error)?;
    let count = count
        + cipher
            .finalize(&mut output[count..])
            .map_err(crypto_error)?;
    if count != 128 {
        return Err(SessionError::msg("Invalid ARD encrypted credential length"));
    }
    Ok(output[..128].try_into().expect("fixed credential length"))
}

pub(crate) async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<(), SessionError> {
    let username = username.unwrap_or_default();
    let password = password.unwrap_or_default();
    validate_credentials(username, password)?;
    stream.write_all(&[SECURITY_TYPE]).await?;
    let generator = stream.read_u16().await?;
    let key_length = usize::from(stream.read_u16().await?);
    // Bound allocations and modular exponentiation before reading server data.
    if !(16..=512).contains(&key_length) {
        return Err(SessionError::msg("Invalid ARD DH key length"));
    }
    let mut modulus = vec![0; key_length];
    let mut peer = vec![0; key_length];
    stream.read_exact(&mut modulus).await?;
    stream.read_exact(&mut peer).await?;
    let modulus = BigNum::from_slice(&modulus).map_err(crypto_error)?;
    let peer = BigNum::from_slice(&peer).map_err(crypto_error)?;
    let generator = BigNum::from_u32(generator.into()).map_err(crypto_error)?;
    let two = BigNum::from_u32(2).map_err(crypto_error)?;
    let mut ctx = BigNumContext::new_secure().map_err(crypto_error)?;
    if modulus.num_bits() < 128 || !modulus.is_odd() {
        return Err(SessionError::msg("Invalid ARD DH modulus"));
    }
    let mut limit = modulus.to_owned().map_err(crypto_error)?;
    limit.sub_word(2).map_err(crypto_error)?;
    if generator < two
        || generator > limit
        || peer < two
        || peer > limit
        || !modulus.is_prime(32, &mut ctx).map_err(crypto_error)?
    {
        return Err(SessionError::msg("Invalid ARD DH parameters"));
    }
    // Uniform private exponent in [2, p-2], with constant-time exponentiation.
    limit.sub_word(1).map_err(crypto_error)?;
    let mut private = BigNum::new_secure().map_err(crypto_error)?;
    limit.rand_range(&mut private).map_err(crypto_error)?;
    private.add_word(2).map_err(crypto_error)?;
    private.set_const_time();
    let mut public = BigNum::new().map_err(crypto_error)?;
    public
        .mod_exp(&generator, &private, &modulus, &mut ctx)
        .map_err(crypto_error)?;
    let mut shared = BigNum::new_secure().map_err(crypto_error)?;
    shared
        .mod_exp(&peer, &private, &modulus, &mut ctx)
        .map_err(crypto_error)?;
    private.clear();
    // ARD hashes the entire field, including leading zero bytes.
    let secret = Zeroizing::new(
        shared
            .to_vec_padded(key_length as i32)
            .map_err(crypto_error)?,
    );
    shared.clear();
    let public = public
        .to_vec_padded(key_length as i32)
        .map_err(crypto_error)?;
    let mut credentials = Zeroizing::new([0; 128]);
    rand_bytes(credentials.as_mut()).map_err(crypto_error)?;
    credentials[..username.len()].copy_from_slice(username.as_bytes());
    credentials[username.len()] = 0;
    credentials[64..64 + password.len()].copy_from_slice(password.as_bytes());
    credentials[64 + password.len()] = 0;
    let encrypted = encrypt_credentials(&secret, &credentials)?;
    stream.write_all(&encrypted).await?;
    stream.write_all(&public).await?;
    stream.flush().await?;
    // Check this ourselves: vnc-rs ignores SecurityResult for replayed None auth.
    if stream.read_u32().await? != 0 {
        return Err(SessionError::msg(
            "Mac login was rejected. Check the username, password, and Screen Sharing permission.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{encrypt_credentials, handshake, validate_credentials};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn credential_cipher_matches_fixed_width_secret_vector() {
        // Independent fixture: Python hashlib.md5(bytes(127) + b'\x04'),
        // OpenSSL CLI enc -aes-128-ecb -nopad, with 0x5a credential padding.
        let mut secret = [0; 128];
        secret[127] = 4;
        let mut credentials = [0x5a; 128];
        credentials[..6].copy_from_slice(b"alice\0");
        credentials[64..71].copy_from_slice(b"secret\0");
        let encrypted = encrypt_credentials(&secret, &credentials).unwrap();
        let actual: String = encrypted.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            actual,
            concat!(
                "b688ebc3ba7889802d934560aff19f8e5b14c915fac67cb836f0caea911bf394",
                "5b14c915fac67cb836f0caea911bf3945b14c915fac67cb836f0caea911bf394",
                "6fde4f62502d59a78bf11a25b940214f5b14c915fac67cb836f0caea911bf394",
                "5b14c915fac67cb836f0caea911bf3945b14c915fac67cb836f0caea911bf394",
            )
        );
    }

    #[test]
    fn credentials_are_validated_as_utf8_bytes_without_truncation() {
        assert!(validate_credentials(&"a".repeat(63), &"密".repeat(21)).is_ok());
        for (user, password) in [
            ("", "password"),
            ("user", ""),
            ("user\0other", "password"),
            ("user", "pass\0word"),
            (&"a".repeat(64), "password"),
            ("user", &"密".repeat(22)),
        ] {
            assert!(validate_credentials(user, password).is_err());
        }
    }

    #[tokio::test]
    async fn invalid_key_lengths_are_rejected_before_reading_payload() {
        for length in [0u16, 15, 513, u16::MAX] {
            let (mut client, mut server) = tokio::io::duplex(16);
            server.write_u16(2).await.unwrap();
            server.write_u16(length).await.unwrap();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                handshake(&mut client, Some("user"), Some("password")),
            )
            .await
            .expect("must not wait for malformed payload")
            .unwrap_err();
            assert!(result.to_string().contains("key length"));
            assert_eq!(server.read_u8().await.unwrap(), 30);
        }
    }

    #[tokio::test]
    async fn degenerate_dh_parameters_are_rejected() {
        for (generator, modulus, public) in [
            (1u16, [0xff; 16], [2; 16]),
            (2, [0; 16], [2; 16]),
            (2, [0xff; 16], [0; 16]),
            (2, [0xff; 16], [0xff; 16]),
            (2, [0xff; 16], [2; 16]), // odd but composite modulus
        ] {
            let (mut client, mut server) = tokio::io::duplex(128);
            server.write_u16(generator).await.unwrap();
            server.write_u16(16).await.unwrap();
            server.write_all(&modulus).await.unwrap();
            server.write_all(&public).await.unwrap();
            let error = handshake(&mut client, Some("user"), Some("password"))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("Invalid ARD DH"));
        }
    }
}
