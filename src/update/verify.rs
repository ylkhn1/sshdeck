//! Проверка подписи файлов выпуска (minisign, Ed25519).
//!
//! Открытый ключ встроен в программу из `packaging/minisign.pub`. Файл
//! принимается, только если подпись сделана этим ключом, сходится с
//! содержимым и в подписанном комментарии стоит ожидаемое имя файла
//! (`file:<имя>`): так подпись от одного файла выпуска (или от другой
//! версии) не подойдёт к другому.

use std::io::Read;
use std::path::Path;

use minisign_verify::{Error, PublicKey, Signature};

/// Ключ, которым подписываются выпуски.
pub const RELEASE_KEY: &str = include_str!("../../packaging/minisign.pub");

/// Проверяет `file` по подписи `signature` (текст `.minisig`) ключом
/// `key` (текст `minisign.pub`). `expected_name` — имя файла выпуска.
pub fn verify_file(
    file: &Path,
    signature: &str,
    expected_name: &str,
    key: &str,
) -> Result<(), String> {
    let failed = |reason: String| tr!("Signature check failed: {}", reason);
    let key = PublicKey::decode(key.trim_start())
        .map_err(|_| failed(tr!("the built-in public key is damaged").to_string()))?;
    let signature = Signature::decode(signature)
        .map_err(|_| failed(tr!("the signature file is damaged").to_string()))?;
    if signed_file_name(signature.trusted_comment()) != Some(expected_name) {
        return Err(failed(
            tr!("the signature belongs to another file").to_string(),
        ));
    }
    let mut verifier = key
        .verify_stream(&signature)
        .map_err(|err| failed(reason(&err)))?;
    let mut input = std::fs::File::open(file).map_err(|err| failed(err.to_string()))?;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|err| failed(err.to_string()))?;
        if read == 0 {
            break;
        }
        verifier.update(&buffer[..read]);
    }
    verifier.finalize().map_err(|err| failed(reason(&err)))
}

/// Имя файла из подписанного комментария `timestamp:…\tfile:<имя>\thashed`
/// (minisign пишет туда имя без каталога; на всякий случай каталог
/// отбрасывается).
fn signed_file_name(trusted_comment: &str) -> Option<&str> {
    trusted_comment
        .split('\t')
        .find_map(|field| field.strip_prefix("file:"))
        .map(|path| path.rsplit('/').next().unwrap_or(path))
}

fn reason(error: &Error) -> String {
    match error {
        Error::UnexpectedKeyId => tr!("the file was signed with a different key").to_string(),
        Error::InvalidSignature => {
            tr!("the file does not match its signature (it was changed or damaged)").to_string()
        }
        Error::UnsupportedLegacyMode | Error::UnexpectedAlgorithm | Error::UnsupportedAlgorithm => {
            tr!("unsupported signature format").to_string()
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minisign::KeyPair;

    /// Ключ разработчика (`packaging/minisign.pub` до замены настоящим) и
    /// подпись им файла `hello.txt` с текстом `hello sshdeck\n`. Секретный
    /// ключ в репозиторий не попадает.
    const DEV_KEY: &str = "untrusted comment: minisign public key: AE5012490955CDF5\n\
                           RWT1zVUJSRJQrjoRFH5t5m/W2UcGO+C3RHWP05/E/5asHuxcHtXnPzPr\n";
    const HELLO_SIG: &str = include_str!("testdata/hello.txt.minisig");

    struct Signer(KeyPair);

    impl Signer {
        fn new() -> Self {
            Self(KeyPair::generate_unencrypted_keypair().unwrap())
        }

        fn key(&self) -> String {
            self.0.pk.to_box().unwrap().into_string()
        }

        fn sign(&self, data: &[u8], name: &str) -> String {
            let comment = format!("timestamp:1763000000\tfile:{name}\thashed");
            minisign::sign(Some(&self.0.pk), &self.0.sk, data, Some(&comment), None)
                .unwrap()
                .into_string()
        }
    }

    fn file_with(data: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asset");
        std::fs::write(&path, data).unwrap();
        (dir, path)
    }

    #[test]
    fn valid_signature_is_accepted() {
        let signer = Signer::new();
        let data = b"release contents".repeat(10_000);
        let (_dir, path) = file_with(&data);
        let signature = signer.sign(&data, "sshdeck_0.5.0-1_amd64.deb");
        verify_file(
            &path,
            &signature,
            "sshdeck_0.5.0-1_amd64.deb",
            &signer.key(),
        )
        .unwrap();
    }

    #[test]
    fn tampered_file_is_refused() {
        let signer = Signer::new();
        let signature = signer.sign(b"original", "a.deb");
        let (_dir, path) = file_with(b"0riginal");
        let error = verify_file(&path, &signature, "a.deb", &signer.key()).unwrap_err();
        assert!(error.contains("does not match"), "{error}");
    }

    #[test]
    fn other_key_is_refused() {
        let signer = Signer::new();
        let stranger = Signer::new();
        let (_dir, path) = file_with(b"data");
        let signature = stranger.sign(b"data", "a.deb");
        let error = verify_file(&path, &signature, "a.deb", &signer.key()).unwrap_err();
        assert!(error.contains("different key"), "{error}");
    }

    #[test]
    fn signature_of_another_file_is_refused() {
        // Подлинная подпись старой версии не подходит к новой.
        let signer = Signer::new();
        let (_dir, path) = file_with(b"data");
        let signature = signer.sign(b"data", "sshdeck_0.4.0-1_amd64.deb");
        let error = verify_file(
            &path,
            &signature,
            "sshdeck_0.5.0-1_amd64.deb",
            &signer.key(),
        )
        .unwrap_err();
        assert!(error.contains("another file"), "{error}");
        // Комментарий без имени файла тоже не годится.
        let bare = minisign::sign(
            Some(&signer.0.pk),
            &signer.0.sk,
            &b"data"[..],
            Some("timestamp:1"),
            None,
        )
        .unwrap()
        .into_string();
        assert!(verify_file(&path, &bare, "a.deb", &signer.key()).is_err());
    }

    #[test]
    fn malformed_signature_or_key_is_refused() {
        let signer = Signer::new();
        let (_dir, path) = file_with(b"data");
        for garbage in [
            "",
            "untrusted comment: x\n",
            "<html>404</html>",
            "a\nb\nc\nd\n",
        ] {
            let error = verify_file(&path, garbage, "a.deb", &signer.key()).unwrap_err();
            assert!(error.contains("damaged"), "{error}");
        }
        // Подписанный комментарий изменён — общая подпись не сходится.
        let signature = signer
            .sign(b"data", "a.deb")
            .replace("timestamp:1763000000", "timestamp:1763000001");
        assert!(verify_file(&path, &signature, "a.deb", &signer.key()).is_err());
        let signature = signer.sign(b"data", "a.deb");
        assert!(verify_file(&path, &signature, "a.deb", "not a key").is_err());
    }

    #[test]
    fn dev_key_fixture_verifies() {
        let (_dir, path) = file_with(b"hello sshdeck\n");
        verify_file(&path, HELLO_SIG, "hello.txt", DEV_KEY).unwrap();
        let (_dir, other) = file_with(b"hello sshdeck!\n");
        assert!(verify_file(&other, HELLO_SIG, "hello.txt", DEV_KEY).is_err());
    }

    #[test]
    fn built_in_key_is_readable() {
        PublicKey::decode(RELEASE_KEY.trim_start()).expect("packaging/minisign.pub");
    }

    #[test]
    fn file_name_from_trusted_comment() {
        assert_eq!(
            signed_file_name("timestamp:1\tfile:sshdeck.deb\thashed"),
            Some("sshdeck.deb")
        );
        assert_eq!(signed_file_name("file:dist/a.rpm"), Some("a.rpm"));
        assert_eq!(signed_file_name("timestamp:1"), None);
    }
}
