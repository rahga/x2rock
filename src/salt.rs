//! The household salt, supplied by the person rather than shipped.
//!
//! `link --from-household` decrypts the music-service tokens a household stores
//! with a key derived from the household id and a fixed 16-byte salt (see
//! `sonos/stored.rs`). That salt originates in Sonos's own software, so x2rock
//! does not carry it: the person who wants the feature finds it in public
//! interoperability work (SoCo PR #1010 publishes it with the method) and hands
//! it over once with `x2rock salt`. It is kept in
//! `$XDG_CONFIG_HOME/x2rock/household-salt` at mode 0600, beside
//! `sonos-integration.json` - configuration the person writes, not state x2rock
//! learns.
//!
//! The binary does hold a fingerprint of the known value: an MD5 over a fixed
//! prefix and the salt, which does not give the salt back. It is only there to
//! catch a mistyped or truncated paste before it turns into a decrypt failure
//! twelve seconds into a household read. `--force` saves a value that does not
//! match, for the day a firmware changes it.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};
use serde_json::json;

use crate::hint::{Code, Hint};
use crate::sonos::stored::Salt;
use crate::store;

const FILE: &str = "household-salt";

/// `md5(FINGERPRINT_PREFIX + salt)` for the salt households are known to use.
const KNOWN: &str = "095e0a55014e681817fcaa84aa34f853";
const FINGERPRINT_PREFIX: &[u8] = b"x2rock household salt";

/// `$XDG_CONFIG_HOME/x2rock/household-salt`.
pub fn path() -> Result<PathBuf> {
    store::config_path(FILE)
}

/// Read a salt out of whatever a person is likely to paste: 32 hex digits, or
/// a byte list copied from source - `0x1a, 0x01, ...` or `"\x1a\x01..."`.
/// Colons, spaces, commas and brackets between bytes are ignored.
pub fn parse(text: &str) -> Result<Salt> {
    let text = text.trim();
    let digits: String = if text.contains("0x") || text.contains("\\x") {
        // Each byte is a `0x` or `\x` and the one or two hex digits after it;
        // everything else - `new byte[] {`, quotes, commas - is the source
        // around them. `0x1` keeps its place as `01`.
        let mut bytes = String::new();
        let mut rest = text;
        while let Some(at) = rest.find("0x").into_iter().chain(rest.find("\\x")).min() {
            let after = &rest[at + 2..];
            let len = after
                .chars()
                .take(2)
                .take_while(char::is_ascii_hexdigit)
                .count();
            if len > 0 {
                bytes.push_str(&format!("{:0>2}", &after[..len]));
            }
            rest = &after[len..];
        }
        bytes
    } else {
        text.chars()
            .filter(|c| !matches!(c, ':' | '-' | ' ' | ','))
            .collect()
    };
    if digits.len() != 32 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("a household salt is 16 bytes: 32 hex digits, or a list of sixteen 0x.. bytes");
    }
    let mut salt = [0u8; 16];
    for (i, byte) in salt.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digits[2 * i..2 * i + 2], 16).expect("checked hex above");
    }
    Ok(salt)
}

/// Whether this is the salt households are known to use.
pub fn is_known(salt: &Salt) -> bool {
    let mut hasher = Md5::new();
    hasher.update(FINGERPRINT_PREFIX);
    hasher.update(salt);
    let digest: [u8; 16] = hasher.finalize().into();
    digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        == KNOWN
}

fn hex(salt: &Salt) -> String {
    salt.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn load_from(path: &Path) -> Result<Option<Salt>> {
    match store::read_optional(path)? {
        Some(text) => parse(&text)
            .with_context(|| format!("reading the salt in {}", path.display()))
            .map(Some),
        None => Ok(None),
    }
}

/// The saved salt, or the `salt_missing` error that says how to supply one.
pub fn require() -> Result<Salt> {
    let path = path()?;
    match load_from(&path)? {
        Some(salt) => Ok(salt),
        None => Err(missing(&path)),
    }
}

/// No `fix`: supplying a salt is a person's step - they find the value and
/// paste it - not a command an agent should run blind.
fn missing(path: &Path) -> anyhow::Error {
    Hint::new(
        format!(
            "reading the household's stored accounts needs the household salt, which x2rock \
             does not ship. Paste it once with: x2rock salt (or x2rock salt < file). \
             It is kept in {} at mode 0600; SoCo PR #1010 publishes the value.",
            path.display()
        ),
        Code::SaltMissing,
        None,
    )
    .with_data(json!({ "path": path.display().to_string() }))
    .into()
}

pub fn save_to(path: &Path, salt: &Salt) -> Result<()> {
    store::write_atomically(path, &format!("{}\n", hex(salt)), store::SECRET)
}

/// `x2rock salt`: save one from stdin, report on the saved one, or forget it.
pub fn run(check: bool, clear: bool, force: bool, json: bool) -> Result<()> {
    let path = path()?;
    if clear {
        match std::fs::remove_file(&path) {
            Ok(()) => println!("Forgot the household salt ({}).", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("No household salt was saved.")
            }
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
        return Ok(());
    }
    if check {
        let saved = load_from(&path)?;
        if json {
            println!(
                "{}",
                json!({
                    "saved": saved.is_some(),
                    "known": saved.as_ref().map(is_known),
                    "path": path.display().to_string(),
                })
            );
        } else {
            match saved {
                None => println!("No household salt is saved. Paste one with: x2rock salt"),
                Some(salt) if is_known(&salt) => {
                    println!("A household salt is saved in {}.", path.display())
                }
                Some(_) => println!(
                    "A household salt is saved in {}, but it is not the one households are \
                     known to use; `link --from-household` will fail to decrypt unless a \
                     firmware has changed it.",
                    path.display()
                ),
            }
        }
        return Ok(());
    }

    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprintln!("Paste the household salt (32 hex digits, or a 0x.. byte list), then Enter:");
    }
    let mut text = String::new();
    if stdin.is_terminal() {
        stdin.read_line(&mut text).context("reading the salt")?;
    } else {
        // A salt is a line; a file mistaken for one should not be buffered whole.
        stdin
            .take(64 * 1024)
            .read_to_string(&mut text)
            .context("reading the salt from stdin")?;
    }
    let salt = parse(&text)?;
    if !is_known(&salt) && !force {
        bail!(
            "that is not the salt households are known to use - check for a typo or a \
             truncated paste. Save it anyway with --force (if a firmware has changed it)."
        );
    }
    save_to(&path, &salt)?;
    println!(
        "Saved the household salt to {}. `x2rock link --from-household` can read the \
         household's accounts now.",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TempDir;

    // Synthetic, like every salt in this tree's tests.
    const SALT: Salt = *b"x2rock-test-salt";

    #[test]
    fn hex_and_byte_lists_parse_to_the_same_salt() {
        let plain = hex(&SALT);
        assert_eq!(parse(&plain).unwrap(), SALT);
        assert_eq!(
            parse(&format!("  {}\n", plain.to_uppercase())).unwrap(),
            SALT
        );
        let colons: Vec<String> = SALT.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(parse(&colons.join(":")).unwrap(), SALT);

        let rust: Vec<String> = SALT.iter().map(|b| format!("0x{b:02x}")).collect();
        assert_eq!(parse(&format!("[{}]", rust.join(", "))).unwrap(), SALT);
        let csharp = format!("new byte[] {{ {} }};", rust.join(","));
        assert_eq!(parse(&csharp).unwrap(), SALT);
        let python: String = SALT.iter().map(|b| format!("\\x{b:02x}")).collect();
        assert_eq!(parse(&format!("b\"{python}\"")).unwrap(), SALT);
        // A single-digit byte keeps its place.
        let mut short = rust.clone();
        short[0] = format!("0x{:x}", SALT[0] & 0x0f);
        assert_eq!(parse(&short.join(", ")).unwrap()[0], SALT[0] & 0x0f);
    }

    #[test]
    fn the_wrong_length_or_non_hex_is_refused() {
        assert!(parse("").is_err());
        assert!(parse(&hex(&SALT)[..30]).is_err());
        assert!(parse(&format!("{}00", hex(&SALT))).is_err());
        assert!(parse(&"zz".repeat(16)).is_err());
    }

    #[test]
    fn a_synthetic_salt_is_not_the_known_one() {
        assert!(!is_known(&SALT));
    }

    #[test]
    fn a_saved_salt_round_trips_at_0600_and_a_missing_one_is_none() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("salt");
        let path = dir.path().join("x2rock").join(FILE);
        assert_eq!(load_from(&path).unwrap(), None);
        save_to(&path, &SALT).unwrap();
        assert_eq!(load_from(&path).unwrap(), Some(SALT));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn a_missing_salt_names_its_code_and_where_it_goes() {
        let err = missing(Path::new("/x/household-salt"));
        let (code, fix) = crate::hint::of(&err);
        assert_eq!(code, Code::SaltMissing);
        assert_eq!(fix, None);
        assert!(err.to_string().contains("x2rock salt"));
    }
}
