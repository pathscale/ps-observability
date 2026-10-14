//! The only file-byte writer in this workspace.
//!
//! Call [`register_secret`] as soon as code receives a secret that may not
//! remain in the environment. Every write also checks secret-named environment
//! variables. A match is refused before the destination is opened.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

#[derive(Clone)]
struct Secret {
    name: String,
    value: Vec<u8>,
}

static REGISTERED_SECRETS: OnceLock<Mutex<Vec<Secret>>> = OnceLock::new();

fn registered_secrets() -> &'static Mutex<Vec<Secret>> {
    REGISTERED_SECRETS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Keep a credential, fetched value, or minted token in the writer's memory
/// guard for as long as this process runs. Values shorter than eight
/// characters are treated as flags rather than secrets.
pub fn register_secret(name: &str, value: &str) {
    let mut secrets = registered_secrets()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    remember_secret(&mut secrets, name, value.as_bytes());
}

/// Create a directory only after checking its path against every known secret.
pub fn create_dir(path: &Path) -> io::Result<()> {
    let secrets = lock_known_secrets();
    refuse_if_secret(path, &[], &secrets)?;
    fs::create_dir(path)
}

/// Create a directory tree only after checking its path against known secrets.
pub fn create_dir_all(path: &Path) -> io::Result<()> {
    let secrets = lock_known_secrets();
    refuse_if_secret(path, &[], &secrets)?;
    fs::create_dir_all(path)
}

/// Create a private directory for temporary control files.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    let secrets = lock_known_secrets();
    refuse_if_secret(path, &[], &secrets)?;
    let mut directory = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(path)
}

/// Write a complete file after checking the bytes against all known secrets.
pub fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let secrets = lock_known_secrets();
    write_file_locked(path, bytes, &secrets)
}

fn write_file_locked(path: &Path, bytes: &[u8], secrets: &[Secret]) -> io::Result<()> {
    refuse_if_secret(path, bytes, secrets)?;
    create_parent(path)?;

    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Append bytes after checking the existing file and the final contents.
pub fn append_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let secrets = lock_known_secrets();
    refuse_if_secret(path, bytes, &secrets)?;
    let existing = match fs::read(path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    let mut final_contents = Vec::with_capacity(existing.len().saturating_add(bytes.len()));
    final_contents.extend_from_slice(&existing);
    final_contents.extend_from_slice(bytes);
    refuse_if_secret(path, &final_contents, &secrets)?;
    create_parent(path)?;

    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Copy a file only after checking its complete contents.
pub fn copy_file(source: &Path, destination: &Path) -> io::Result<()> {
    let secrets = lock_known_secrets();
    refuse_if_secret(source, &[], &secrets)?;
    let bytes = fs::read(source)?;
    write_file_locked(destination, &bytes, &secrets)
}

/// Rename a file only after checking the contents that would be moved.
pub fn rename_file(source: &Path, destination: &Path) -> io::Result<()> {
    let secrets = lock_known_secrets();
    refuse_if_secret(source, &[], &secrets)?;
    refuse_if_secret(destination, &[], &secrets)?;
    let bytes = fs::read(source)?;
    refuse_if_secret(destination, &bytes, &secrets)?;
    create_parent(destination)?;
    fs::rename(source, destination)
}

fn create_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn lock_known_secrets() -> MutexGuard<'static, Vec<Secret>> {
    let mut secrets = registered_secrets()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut environment: Vec<_> = env::vars_os().collect();
    environment.sort_by(|left, right| left.0.cmp(&right.0));

    for (name, value) in environment {
        let name = name.to_string_lossy().into_owned();
        let is_storage_seed = name == "QA_INSPECT_STORAGE";
        let value = value.as_os_str().as_encoded_bytes();
        if is_storage_seed
            && let Ok(entries) =
                serde_json::from_slice::<std::collections::BTreeMap<String, String>>(value)
        {
            for (key, entry) in entries {
                remember_secret(&mut secrets, &name, key.as_bytes());
                remember_secret(&mut secrets, &name, entry.as_bytes());
            }
            continue;
        }
        if is_secret_name(&name) || is_storage_seed {
            remember_secret(&mut secrets, &name, value);
        }
    }
    secrets
}

fn remember_secret(secrets: &mut Vec<Secret>, name: &str, value: &[u8]) {
    if is_long_enough(value)
        && !secrets
            .iter()
            .any(|secret| secret.name == name && secret.value.as_slice() == value)
    {
        secrets.push(Secret {
            name: name.to_owned(),
            value: value.to_vec(),
        });
    }
}

fn is_secret_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    const SECRET_MARKERS: [&str; 13] = [
        "USERNAME",
        "PASSWORD",
        "PASS",
        "PASSPHRASE",
        "SEED",
        "KEY",
        "TOKEN",
        "SECRET",
        "SESSION",
        "COOKIE",
        "JWT",
        "BEARER",
        "AUTHORIZATION",
    ];
    if SECRET_MARKERS.iter().any(|marker| name.contains(marker)) || name.contains("CREDENTIAL") {
        return true;
    }

    let is_url = name.ends_with("_URL");
    let is_dsn = name.ends_with("_DSN");
    let is_database = [
        "DATABASE",
        "DB",
        "POSTGRES",
        "POSTGRESQL",
        "PG",
        "MYSQL",
        "MARIADB",
        "MSSQL",
        "SQLSERVER",
        "SQLITE",
        "RDS",
        "REDIS",
        "MONGO",
        "MONGODB",
        "COCKROACH",
        "CLICKHOUSE",
        "ELASTIC",
        "ORACLE",
        "CASSANDRA",
        "NEO4J",
        "INFLUX",
        "SNOWFLAKE",
        "DYNAMODB",
    ]
    .iter()
    .any(|marker| name.contains(marker));
    is_dsn || (is_url && is_database)
}

fn is_long_enough(value: &[u8]) -> bool {
    std::str::from_utf8(value)
        .map(|value| value.chars().count() >= 8)
        .unwrap_or_else(|_| value.len() >= 8)
}

fn refuse_if_secret(path: &Path, bytes: &[u8], secrets: &[Secret]) -> io::Result<()> {
    let path_bytes = path.as_os_str().as_encoded_bytes();
    for secret in secrets {
        if secret
            .forms()
            .iter()
            .any(|form| contains(bytes, form) || contains(path_bytes, form))
        {
            let path = safe_path(path, secrets);
            let name = safe_name(&secret.name, secrets);
            return Err(io::Error::other(format!(
                "refusing to write path {path:?}: secret {name} would be written; secrets live only in memory, pipes and child-process environment; never on disk"
            )));
        }
    }
    Ok(())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn safe_path(path: &Path, secrets: &[Secret]) -> String {
    let mut displayed = path.to_string_lossy().into_owned();
    let mut forms: Vec<Vec<u8>> = secrets.iter().flat_map(Secret::forms).collect();
    forms.sort_by_key(|form| std::cmp::Reverse(form.len()));
    for form in forms {
        if let Ok(form) = std::str::from_utf8(&form)
            && !form.is_empty()
        {
            displayed = displayed.replace(form, "[redacted]");
        }
    }
    displayed
}

fn safe_name(name: &str, secrets: &[Secret]) -> String {
    let mut name = name.to_owned();
    for secret in secrets {
        for form in secret.forms() {
            if let Ok(form) = std::str::from_utf8(&form)
                && !form.is_empty()
            {
                for variant in [
                    form.to_owned(),
                    form.to_ascii_uppercase(),
                    form.to_ascii_lowercase(),
                ] {
                    name = name.replace(&variant, "[REDACTED]");
                }
            }
        }
    }
    let name: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() {
        "SECRET".to_owned()
    } else {
        name
    }
}

impl Secret {
    fn forms(&self) -> Vec<Vec<u8>> {
        let mut forms = vec![self.value.clone()];
        forms.extend(url_encoded_forms(&self.value));
        forms.extend(base64_forms(&self.value));
        forms.extend(hex_forms(&self.value));
        if let Ok(value) = std::str::from_utf8(&self.value) {
            if let Ok(quoted) = serde_json::to_string(value) {
                forms.push(quoted.as_bytes().to_vec());
                forms.push(quoted.as_bytes()[1..quoted.len() - 1].to_vec());
            }
            let escaped = json_unicode_escaped(value, false);
            forms.push(escaped.as_bytes().to_vec());
            forms.push(format!("\"{escaped}\"").into_bytes());
            forms.push(escaped.replace('/', "\\/").into_bytes());
            forms.push(json_unicode_escaped(value, true).into_bytes());
        }
        forms
    }
}

fn url_encoded_forms(value: &[u8]) -> Vec<Vec<u8>> {
    let mut forms = Vec::new();
    for flavor in [
        UrlFlavor::Form,
        UrlFlavor::Component,
        UrlFlavor::StrictComponent,
        UrlFlavor::Full,
    ] {
        forms.push(percent_encode(value, flavor, false));
        forms.push(percent_encode(value, flavor, true));
    }
    forms
}

#[derive(Clone, Copy)]
enum UrlFlavor {
    Form,
    Component,
    StrictComponent,
    Full,
}

fn percent_encode(value: &[u8], flavor: UrlFlavor, lowercase: bool) -> Vec<u8> {
    let digits = if lowercase {
        b"0123456789abcdef"
    } else {
        b"0123456789ABCDEF"
    };
    let mut encoded = Vec::with_capacity(value.len().saturating_mul(3));
    for byte in value {
        let plain = byte.is_ascii_alphanumeric()
            || match flavor {
                UrlFlavor::Form => matches!(byte, b'*' | b'-' | b'.' | b'_'),
                UrlFlavor::Component => matches!(
                    byte,
                    b'-' | b'_' | b'.' | b'~' | b'!' | b'\'' | b'(' | b')' | b'*'
                ),
                UrlFlavor::StrictComponent => matches!(byte, b'-' | b'_' | b'.' | b'~'),
                UrlFlavor::Full => false,
            };
        if plain {
            encoded.push(*byte);
        } else if matches!(flavor, UrlFlavor::Form) && *byte == b' ' {
            encoded.push(b'+');
        } else {
            encoded.push(b'%');
            encoded.push(digits[(byte >> 4) as usize]);
            encoded.push(digits[(byte & 0x0f) as usize]);
        }
    }
    encoded
}

fn hex_forms(value: &[u8]) -> Vec<Vec<u8>> {
    let mut forms = Vec::with_capacity(2);
    for digits in [b"0123456789abcdef", b"0123456789ABCDEF"] {
        let mut encoded = Vec::with_capacity(value.len().saturating_mul(2));
        for &byte in value {
            encoded.push(digits[(byte >> 4) as usize]);
            encoded.push(digits[(byte & 0x0f) as usize]);
        }
        forms.push(encoded);
    }
    forms
}

fn base64_forms(value: &[u8]) -> Vec<Vec<u8>> {
    const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    const URL_SAFE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut forms = vec![
        base64_encode(value, STANDARD, true),
        base64_encode(value, STANDARD, false),
        base64_encode(value, URL_SAFE, true),
        base64_encode(value, URL_SAFE, false),
    ];
    if value.len() >= 6 {
        for alphabet in [STANDARD, URL_SAFE] {
            for offset in 0_usize..3 {
                let mut aligned = vec![0_u8; offset];
                aligned.extend_from_slice(value);
                let encoded = base64_encode(&aligned, alphabet, false);

                // Keep only sextets composed entirely of secret bits.
                let start = (offset * 8).div_ceil(6);
                let end = (aligned.len() / 3) * 4
                    + match aligned.len() % 3 {
                        0 => 0,
                        1 => 1,
                        _ => 2,
                    };
                forms.push(encoded[start..end].to_vec());
            }
        }
    }
    forms
}

fn base64_encode(value: &[u8], alphabet: &[u8; 64], padding: bool) -> Vec<u8> {
    let capacity = (value.len().saturating_add(2) / 3).saturating_mul(4);
    let mut encoded = Vec::with_capacity(capacity);
    for chunk in value.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(alphabet[(first >> 2) as usize]);
        encoded.push(alphabet[(((first & 0x03) << 4) | (second >> 4)) as usize]);
        if chunk.len() > 1 {
            encoded.push(alphabet[(((second & 0x0f) << 2) | (third >> 6)) as usize]);
        } else if padding {
            encoded.push(b'=');
        }
        if chunk.len() > 2 {
            encoded.push(alphabet[(third & 0x3f) as usize]);
        } else if padding {
            encoded.push(b'=');
        }
    }
    encoded
}

fn json_unicode_escaped(value: &str, uppercase_hex: bool) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\u{08}' => escaped.push_str("\\b"),
            '\u{0c}' => escaped.push_str("\\f"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character < ' ' => {
                push_unicode_escape(&mut escaped, character as u16, uppercase_hex)
            }
            character if character.is_ascii() => escaped.push(character),
            character => {
                let mut units = [0; 2];
                for unit in character.encode_utf16(&mut units).iter() {
                    push_unicode_escape(&mut escaped, *unit, uppercase_hex);
                }
            }
        }
    }
    escaped
}

fn push_unicode_escape(escaped: &mut String, unit: u16, uppercase_hex: bool) {
    if uppercase_hex {
        escaped.push_str(&format!("\\u{unit:04X}"));
    } else {
        escaped.push_str(&format!("\\u{unit:04x}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn secret(label: &str) -> String {
        format!("writer test/{label}/{}", std::process::id())
    }

    fn temp_path(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the epoch")
            .as_nanos();
        env::temp_dir().join(format!(
            "guarded-writer-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn assert_refused(label: &str, value: &str, bytes: &[u8]) {
        register_secret("WRITER_TEST_PASSWORD", value);
        let path = temp_path(label);
        let error = write_file(&path, bytes).expect_err("secret-bearing bytes must be refused");
        let message = error.to_string();
        assert!(message.contains(path.to_string_lossy().as_ref()));
        assert!(message.contains("WRITER_TEST_PASSWORD"));
        assert!(message.contains(
            "secrets live only in memory, pipes and child-process environment; never on disk"
        ));
        assert!(!message.contains(value));
        assert!(!path.exists());
    }

    #[test]
    fn refuses_raw_secret_bytes() {
        let value = secret("raw");
        assert_refused("raw", &value, value.as_bytes());
    }

    #[test]
    fn refuses_url_encoded_secret_bytes() {
        let value = secret("url");
        let encoded = percent_encode(value.as_bytes(), UrlFlavor::Form, false);
        assert_refused("url", &value, &encoded);
    }

    #[test]
    fn refuses_json_escaped_secret_bytes() {
        let value = format!("writer \"quoted\"/{}/{}", "\\", std::process::id());
        let encoded = serde_json::to_string(&value).expect("a string serializes as JSON");
        assert_refused("json", &value, encoded.as_bytes());
    }

    #[test]
    fn refuses_base64_secret_bytes() {
        let value = secret("base64");
        let encoded = base64_encode(
            value.as_bytes(),
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
            true,
        );
        assert_refused("base64", &value, &encoded);
    }

    #[test]
    fn names_the_secret_and_path_without_echoing_the_value() {
        let value = secret("error");
        let path = temp_path("error");
        register_secret("WRITER_TEST_TOKEN", &value);
        let error =
            write_file(&path, value.as_bytes()).expect_err("secret-bearing bytes must fail");
        let message = error.to_string();
        assert!(message.contains(path.to_string_lossy().as_ref()));
        assert!(message.contains("WRITER_TEST_TOKEN"));
        assert!(!message.contains(&value));
        assert!(message.contains("never on disk"));
    }

    #[test]
    fn a_secret_in_the_destination_path_is_redacted_from_the_error() {
        let value = secret("path");
        register_secret("WRITER_TEST_SECRET", &value);
        let path = env::temp_dir().join(format!("guarded-writer-{value}"));
        let error = write_file(&path, b"ordinary report output")
            .expect_err("secret-bearing destination paths must be refused");
        let message = error.to_string();
        assert!(message.contains("[redacted]"));
        assert!(!message.contains(&value));
        assert!(message.contains("WRITER_TEST_SECRET"));
        assert!(!path.exists());
    }

    #[test]
    fn writer_refusal_subprocess_child() {
        if env::var_os("GUARDED_WRITER_CHILD_MODE").is_none() {
            return;
        }
        let path = std::path::PathBuf::from(
            env::var_os("WRITER_CHILD_DESTINATION").expect("child destination is set"),
        );
        let value = match env::var("QA_INSPECT_STORAGE") {
            Ok(storage) => {
                serde_json::from_str::<std::collections::BTreeMap<String, String>>(&storage)
                    .expect("child storage seed is valid")
                    .into_values()
                    .next()
                    .expect("child storage seed has a value")
            }
            Err(_) => env::var("WRITER_TEST_PASSWORD").expect("child secret is set"),
        };
        match write_file(&path, value.as_bytes()) {
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
            Ok(()) => std::process::exit(0),
        }
    }

    #[test]
    fn secret_write_exits_nonzero_in_a_subprocess() {
        let value = secret("subprocess");
        let path = temp_path("subprocess");
        let output = Command::new(env::current_exe().expect("test executable path"))
            .args([
                "--exact",
                "guarded_writer::tests::writer_refusal_subprocess_child",
                "--nocapture",
            ])
            .env("GUARDED_WRITER_CHILD_MODE", "1")
            .env("WRITER_CHILD_DESTINATION", &path)
            .env("WRITER_TEST_PASSWORD", &value)
            .output()
            .expect("run the guarded writer CLI child");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains(path.to_string_lossy().as_ref()));
        assert!(stderr.contains("WRITER_TEST_PASSWORD"));
        assert!(!stderr.contains(&value));
        assert!(!path.exists());
    }

    #[test]
    fn storage_seed_secret_is_refused_by_the_child_process() {
        let value = secret("storage");
        let path = temp_path("storage");
        let storage = serde_json::to_string(&std::collections::BTreeMap::from([(
            "opaque-key",
            value.as_str(),
        )]))
        .expect("storage seed serializes");
        let output = Command::new(env::current_exe().expect("test executable path"))
            .args([
                "--exact",
                "guarded_writer::tests::writer_refusal_subprocess_child",
                "--nocapture",
            ])
            .env("GUARDED_WRITER_CHILD_MODE", "1")
            .env("WRITER_CHILD_DESTINATION", &path)
            .env("QA_INSPECT_STORAGE", storage)
            .output()
            .expect("run the guarded writer child with a storage seed");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains(path.to_string_lossy().as_ref()));
        assert!(stderr.contains("QA_INSPECT_STORAGE"));
        assert!(!stderr.contains(&value));
        assert!(!path.exists());
    }

    #[test]
    fn database_urls_and_secret_name_markers_are_recognized() {
        for name in [
            "APP_USERNAME",
            "APP_PASSWORD",
            "APP_PASS",
            "APP_PASSPHRASE",
            "APP_SEED",
            "APP_KEY",
            "APP_TOKEN",
            "APP_SECRET",
            "APP_SESSION",
            "APP_COOKIE",
            "APP_JWT",
            "APP_BEARER",
            "APP_AUTHORIZATION",
            "APP_CREDENTIALS",
            "DATABASE_URL",
            "DB_DSN",
            "RDS_URL",
            "PG_DSN",
            "MSSQL_URL",
            "MONGODB_URL",
        ] {
            assert!(is_secret_name(name));
        }
        assert!(!is_secret_name("FEATURE_FLAG"));
        assert!(!is_secret_name("PUBLIC_SITE_URL"));
    }

    #[test]
    fn values_shorter_than_eight_characters_are_flags() {
        assert!(!is_long_enough(b"1"));
        assert!(!is_long_enough(b"enabled"));
        assert!(is_long_enough(b"long-enough"));
    }

    #[test]
    fn non_secret_file_write_succeeds() {
        let path = temp_path("success");
        let bytes = b"ordinary report output";
        write_file(&path, bytes).expect("non-secret bytes are written");
        assert_eq!(fs::read(&path).expect("written file is readable"), bytes);
        fs::remove_file(path).expect("test output is removed");
    }
}
