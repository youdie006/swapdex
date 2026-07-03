//! Credential bytes wrapper: never prints, zeroizes on drop. The ONLY way to
//! read the bytes is `expose()`, which is easy to grep for in review. No
//! `#[derive(Debug)]` in the credential path relies on this type.

use zeroize::Zeroize;

pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: Vec<u8>) -> Self {
        Secret(bytes)
    }
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_never_prints_its_bytes() {
        let s = Secret::new(b"sk-ant-oat01-TOPSECRET".to_vec());
        assert_eq!(format!("{s:?}"), "Secret([redacted])");
        assert_eq!(format!("{s}"), "[redacted]");
        assert!(!format!("{s:?}{s}").contains("TOPSECRET"));
        assert_eq!(s.expose(), b"sk-ant-oat01-TOPSECRET");
    }
}
