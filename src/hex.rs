use std::fmt::Write;

pub(crate) fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().fold(String::new(), |mut out, byte| {
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::hex;
    use sha2::{Digest, Sha256};

    #[test]
    fn hex_digest_is_lowercase_sha256_hex() {
        assert_eq!(
            hex(Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
