//! Validating ARD mock shared by the manual server and TCP integration tests.
//! Uses OpenSSL's DH API independently of the client's BN implementation.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use openssl::bn::BigNum;
use openssl::dh::Dh;
use openssl::hash::{MessageDigest, hash};
use openssl::symm::{Cipher, Crypter, Mode};
use zeroize::Zeroizing;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn field_matches(field: &[u8], expected: &str) -> bool {
    field.iter().position(|b| *b == 0).is_some_and(|end| {
        end == expected.len() && openssl::memcmp::eq(&field[..end], expected.as_bytes())
    })
}

pub fn authenticate(sock: &mut TcpStream, username: &str, password: &str) -> io::Result<()> {
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    sock.set_write_timeout(Some(Duration::from_secs(5)))?;
    // Apple's version and the exact security offer from the reported failure.
    sock.write_all(b"RFB 003.889\n")?;
    let mut version = [0; 12];
    sock.read_exact(&mut version)?;
    if &version != b"RFB 003.008\n" {
        return Err(invalid("expected RFB 3.8 client version"));
    }
    sock.write_all(&[4, 30, 33, 36, 35])?;
    let mut choice = [0];
    sock.read_exact(&mut choice)?;
    if choice != [30] {
        return Err(invalid("client did not select ARD"));
    }

    let dh = Dh::from_pqg(
        BigNum::get_rfc2409_prime_1024()?,
        None,
        BigNum::from_u32(2)?,
    )?
    .generate_key()?;
    sock.write_all(&2u16.to_be_bytes())?;
    sock.write_all(&128u16.to_be_bytes())?;
    sock.write_all(&dh.prime_p().to_vec_padded(128)?)?;
    sock.write_all(&dh.public_key().to_vec_padded(128)?)?;
    let mut encrypted = [0; 128];
    let mut public = [0; 128];
    sock.read_exact(&mut encrypted)?;
    sock.read_exact(&mut public)?;
    let public = BigNum::from_slice(&public)?;
    let secret = Zeroizing::new(dh.compute_key(&public)?);
    // DH_compute_key strips zeroes, while ARD hashes the fixed-width field.
    let mut padded = Zeroizing::new([0; 128]);
    padded[128 - secret.len()..].copy_from_slice(&secret);
    let key = Zeroizing::new(hash(MessageDigest::md5(), padded.as_ref())?.to_vec());
    let mut cipher = Crypter::new(Cipher::aes_128_ecb(), Mode::Decrypt, &key, None)?;
    cipher.pad(false);
    let mut clear = Zeroizing::new([0; 144]);
    let count = cipher.update(&encrypted, clear.as_mut())?;
    let count = count + cipher.finalize(&mut clear[count..])?;
    let accepted = count == 128
        && field_matches(&clear[..64], username)
        && field_matches(&clear[64..128], password);
    if accepted {
        sock.write_all(&0u32.to_be_bytes())?;
        sock.set_read_timeout(None)?;
        sock.set_write_timeout(None)?;
        Ok(())
    } else {
        let reason = b"Invalid mock credentials";
        let mut failure = 1u32.to_be_bytes().to_vec();
        failure.extend_from_slice(&(reason.len() as u32).to_be_bytes());
        failure.extend_from_slice(reason);
        sock.write_all(&failure)?;
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "ARD credentials rejected",
        ))
    }
}
