pub fn fake_config_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("config tempdir");
    for config_dir in [
        home.path().join("libertai"),
        home.path()
            .join("Library")
            .join("Application Support")
            .join("libertai"),
    ] {
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            "[auth]\napi_key = \"LTAI_sk_probe_config_00000000000000000000\"\n",
        )
        .unwrap();
    }
    home
}
