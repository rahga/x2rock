//! A built-in salt is taken before the saved file. A test binary of its own:
//! the salt is set once per process, and the library's other tests expect none.

#[test]
fn a_built_in_salt_is_taken_before_the_saved_file() {
    let config = std::env::temp_dir().join(format!("x2rock-salt-test-{}", std::process::id()));
    std::fs::create_dir_all(&config).unwrap();
    // SAFETY: the only test in this binary, before any thread reads the variable.
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &config) };
    assert!(
        x2rock::salt::require().is_err(),
        "no file, nothing built in"
    );
    // Synthetic, not the speakers' salt.
    let salt = *b"x2rock-test-salt";
    x2rock::salt::set_built_in(salt);
    assert_eq!(x2rock::salt::require().unwrap(), salt);
    let _ = std::fs::remove_dir_all(&config);
}
