fn main() {
    slint_build::compile("ui/app.slint").expect("ui/app.slint compiles");
    // The icon Explorer shows for the .exe.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("dist/x2rock-deck.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("the Windows icon resource compiles");
    }
    built_in_salt();
}

/// A speaker salt to build in, from the file X2ROCK_SPEAKER_SALT_FILE names,
/// when it names one and that file is there; none otherwise, which is every
/// build from a plain checkout. The salt is not in this repository.
///
/// The variable, not the file, is the switch for watching: Cargo reruns a
/// build script on every build while a path it watches is missing, and this
/// one compiles the UI. So only a build that names a file watches anything.
/// While that file is there, it is watched, so deleting it (or a symlink to
/// it) or changing it makes the next build follow. While it is not, a path
/// that never exists is watched instead, so every build looks again: Cargo
/// would miss the file coming back, since a symlink restored to an older file
/// carries that file's older time.
fn built_in_salt() {
    println!("cargo:rerun-if-env-changed=X2ROCK_SPEAKER_SALT_FILE");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let file = std::env::var_os("X2ROCK_SPEAKER_SALT_FILE").filter(|f| !f.is_empty());
    let salt = file.and_then(|file| {
        let path = std::path::PathBuf::from(file);
        let Ok(text) = std::fs::read_to_string(&path) else {
            println!(
                "cargo:rerun-if-changed={}",
                out.join("no-salt-yet").display()
            );
            return None;
        };
        println!("cargo:rerun-if-changed={}", path.display());
        let hex = text.trim();
        let valid = hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit());
        assert!(
            valid,
            "{} must hold a speaker salt as 32 hex digits",
            path.display()
        );
        Some(
            (0..16)
                .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
                .collect::<Vec<u8>>(),
        )
    });
    let code = match &salt {
        Some(bytes) => format!("pub const SPEAKER_SALT: Option<[u8; 16]> = Some({bytes:?});\n"),
        None => "pub const SPEAKER_SALT: Option<[u8; 16]> = None;\n".to_owned(),
    };
    std::fs::write(out.join("built_in.rs"), code).expect("OUT_DIR is writable");
}
