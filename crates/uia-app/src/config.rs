// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::SecretStore;

    /// `OPENAI_API_KEY` is process-global and the harness runs tests in
    /// parallel, so every test that sets or clears it must hold this first.
    /// Without it these tests pass alone and fail intermittently together —
    /// the worst possible failure mode for a suite that is the acceptance
    /// evidence for every stage.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Ignoring a poisoned lock is right here: a panicking test tells us
    /// nothing about whether the *next* test may touch the environment.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn engine_choice_round_trips_through_its_wire_string() {
        assert_eq!("openai".parse(), Ok(EngineChoice::OpenAi));
        assert_eq!("bedrock".parse(), Ok(EngineChoice::Bedrock));
        assert_eq!("foundry".parse(), Ok(EngineChoice::Foundry));
        assert_eq!(EngineChoice::OpenAi.as_wire(), "openai");
        assert_eq!(EngineChoice::Bedrock.as_wire(), "bedrock");
        assert_eq!(EngineChoice::Foundry.as_wire(), "foundry");
    }

    /// The rename's whole safety story in one test: everything a Bedrock user
    /// already has on disk — a `uia-engine.json` written by `set_engine`,
    /// a `[nova]` section in their `uia.toml` — must still resolve to the
    /// same engine. Deleting any of these three assertions silently resets a
    /// real user's configuration to defaults on upgrade.
    #[test]
    fn the_legacy_nova_identifiers_still_resolve_to_bedrock() {
        // 1. The IPC/sidecar wire id.
        assert_eq!("nova".parse(), Ok(EngineChoice::Bedrock));

        // 2. The serde representation `uia-engine.json` persists.
        let legacy: EngineChoice = serde_json::from_str("\"nova\"").unwrap();
        assert_eq!(legacy, EngineChoice::Bedrock);

        // 3. The `[nova]` TOML section, aliased onto `config.bedrock`.
        let toml = r#"
            [engine]
            default = "nova"
            [nova]
            region = "us-west-2"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.engine.default, EngineChoice::Bedrock);
        assert_eq!(
            c.bedrock.region, "us-west-2",
            "a legacy [nova] section must not degrade to the default region"
        );
    }

    /// The new spelling of the same three surfaces.
    #[test]
    fn the_bedrock_section_and_wire_id_parse_under_the_new_name() {
        let toml = r#"
            [engine]
            default = "bedrock"
            [bedrock]
            region = "us-east-1"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.engine.default, EngineChoice::Bedrock);
        assert_eq!(c.bedrock.region, "us-east-1");
    }

    #[test]
    fn an_unknown_engine_wire_string_is_a_clear_error_not_a_panic() {
        let err = "gemini".parse::<EngineChoice>().unwrap_err();
        assert!(err.contains("gemini"), "got: {err}");
    }

    #[test]
    fn a_minimal_config_parses_with_sensible_defaults() {
        let toml = r#"
            [openai]
            api_key = "test-key"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            c.engine.default,
            EngineChoice::OpenAi,
            "must default to openai"
        );
        assert_eq!(c.bedrock.region, "ap-northeast-1", "must default to Tokyo");
        // Was `None` while SP1 shipped NullMemory only. The default moved
        // deliberately: the app now disconnects on purpose — an idle timeout,
        // Settings opening, a session rotated before its provider's ceiling —
        // and with no memory each of those resumes with the assistant having
        // forgotten the conversation. `none` is still available for anyone who
        // would rather nothing were written down.
        assert_eq!(
            c.memory.backend,
            MemoryBackend::Local,
            "history must survive the app's own reconnects by default"
        );
        assert_eq!(
            c.openai.model, "gpt-realtime-2.1-mini",
            "must default to the mini model"
        );
        assert!(
            c.openai.enable_audio_cache,
            "audio caching must default on (PLAN.md FD14: a standing constraint,              not a task — no stage may turn it off, and S6 measures its hit rate)"
        );
        assert!(c.audio.aec_enabled, "OS echo cancellation must default on");
    }

    #[test]
    fn os_aec_can_be_disabled_via_config() {
        let toml = r#"
            [audio]
            aec_enabled = false
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert!(!c.audio.aec_enabled);
    }

    #[test]
    fn input_and_output_devices_default_to_trusting_the_os_default() {
        let c = Config::default();
        assert_eq!(c.audio.input_device, None);
        assert_eq!(c.audio.output_device, None);
    }

    #[test]
    fn input_and_output_devices_can_be_pinned_explicitly() {
        let toml = r#"
            [audio]
            input_device = "HD Pro Webcam C920"
            output_device = "Creative Pebble X"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.audio.input_device.as_deref(), Some("HD Pro Webcam C920"));
        assert_eq!(c.audio.output_device.as_deref(), Some("Creative Pebble X"));
    }

    #[test]
    fn barge_in_threshold_defaults_to_none_leaving_the_session_default_in_place() {
        let c = Config::default();
        assert_eq!(c.audio.barge_in_threshold, None);
    }

    #[test]
    fn barge_in_threshold_can_be_tuned_via_config() {
        let toml = r#"
            [audio]
            barge_in_threshold = 0.02
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.audio.barge_in_threshold, Some(0.02));
    }

    #[test]
    fn bedrock_region_is_explicit_and_never_inherited_from_the_aws_profile() {
        let toml = r#"
            [engine]
            default = "bedrock"
            [bedrock]
            region = "us-west-2"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.bedrock.region, "us-west-2");
    }

    #[test]
    fn an_unsupported_bedrock_region_is_rejected_at_load_time() {
        let toml = r#"
            [engine]
            default = "bedrock"
            [bedrock]
            region = "ap-southeast-2"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert!(
            c.validate().is_err(),
            "Sydney must be rejected before connect"
        );
    }

    /// An upgrading user's `[[mcp.servers]]` entries are ignored rather than
    /// rejected, so without a notice their servers just stop working and
    /// nothing says why. The notice fires on the section in any of the forms
    /// it was ever documented in — and stays quiet for a config that never
    /// had one, or that only mentions it in a comment, which is exactly what
    /// the replacement text in `uia.example.toml` does.
    #[test]
    fn a_leftover_mcp_section_is_noticed_and_nothing_else_is() {
        for present in [
            "[[mcp.servers]]\nname = \"clock\"\n",
            "[mcp]\n",
            "[mcp.servers]\n",
            "[openai]\nmodel = \"x\"\n\n  [[mcp.servers]]\n  name = \"clock\"\n",
        ] {
            assert!(
                mentions_removed_mcp_section(present),
                "must notice: {present:?}"
            );
        }

        for absent in [
            "",
            "[openai]\nmodel = \"gpt\"\n[memory]\nbackend = \"file\"\n",
            // The replacement comment this task put in `uia.example.toml`.
            "# MCP servers are no longer configured here. Add them in the app's\n\
             # Settings -> MCP tab.\n",
            // The commented-out example the old file shipped.
            "# [[mcp.servers]]\n# name = \"clock\"\n",
            // A section whose name merely starts with the same letters.
            "[mcpb]\nfoo = 1\n",
        ] {
            assert!(
                !mentions_removed_mcp_section(absent),
                "must stay quiet: {absent:?}"
            );
        }
    }

    #[test]
    fn env_var_wins_over_key_file_for_openai_credentials() {
        let _env = env_guard();
        // SAFETY: single-threaded test-local env mutation, restored before return.
        unsafe { std::env::set_var("OPENAI_API_KEY", "from-env") };
        let openai = OpenAiSection {
            key_file: Some("/nonexistent/path.key".into()),
            ..Default::default()
        };
        let store = crate::secrets::FakeSecretStore::new();
        let result = openai.resolve_api_key(&store);
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        assert_eq!(result.unwrap(), "from-env");
    }

    #[test]
    fn key_file_is_read_and_trimmed_when_env_var_is_absent() {
        let _env = env_guard();
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let dir = std::env::temp_dir();
        let path = dir.join(format!("uia-test-key-{}.key", std::process::id()));
        std::fs::write(&path, "  from-file-key\n").unwrap();
        let openai = OpenAiSection {
            key_file: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let store = crate::secrets::FakeSecretStore::new();
        let result = openai.resolve_api_key(&store);
        std::fs::remove_file(&path).ok();
        assert_eq!(result.unwrap(), "from-file-key");
    }

    #[test]
    fn missing_credentials_are_a_clear_config_error_not_a_panic() {
        let _env = env_guard();
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let openai = OpenAiSection::default();
        let store = crate::secrets::FakeSecretStore::new();
        assert!(openai.resolve_api_key(&store).is_err());
    }

    #[test]
    fn an_inline_api_key_is_actually_read() {
        // Declared and documented in uia.example.toml since S12, silently
        // ignored by resolve_api_key until S14 composed the real app.
        let _env = env_guard();
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let openai = OpenAiSection {
            api_key: Some("  inline-key\n".into()),
            ..Default::default()
        };
        let store = crate::secrets::FakeSecretStore::new();
        assert_eq!(openai.resolve_api_key(&store).unwrap(), "inline-key");
    }

    #[test]
    fn env_vars_win_over_inline_nova_credentials() {
        let _env = env_guard();
        // SAFETY: single-threaded test-local env mutation, restored before return.
        unsafe {
            std::env::set_var("AWS_ACCESS_KEY_ID", "env-key");
            std::env::set_var("AWS_SECRET_ACCESS_KEY", "env-secret");
        }
        let nova = BedrockSection {
            access_key_id: Some("inline-key".into()),
            secret_access_key: Some("inline-secret".into()),
            ..Default::default()
        };
        let store = crate::secrets::FakeSecretStore::new();
        let result = nova.resolve_credentials(&store);
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
        let (access_key_id, secret_access_key) = result.unwrap();
        assert_eq!(access_key_id, "env-key");
        assert_eq!(secret_access_key, "env-secret");
    }

    /// The failure this guards against is silent and expensive: a user who
    /// stored AWS keys before the rename has them in their OS keychain under
    /// `nova_*`, and if the read only ever looked for `aws_*` the app would
    /// fall through to "no credentials" while the secrets sat right there,
    /// unreachable. Nothing else in the suite covers the fallback.
    #[test]
    fn aws_keys_stored_under_the_pre_rename_account_ids_are_still_found() {
        let _env = env_guard();
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
        let store = crate::secrets::FakeSecretStore::new();
        store.set("nova_access_key_id", "legacy-key").unwrap();
        store
            .set("nova_secret_access_key", "legacy-secret")
            .unwrap();

        let (access_key_id, secret_access_key) = BedrockSection::default()
            .resolve_credentials(&store)
            .expect("a pre-rename keyring entry must still resolve");
        assert_eq!(access_key_id, "legacy-key");
        assert_eq!(secret_access_key, "legacy-secret");
    }

    /// ...and the canonical ids win when both are present, so a user who
    /// re-saves their keys is not stuck reading the stale pair forever.
    #[test]
    fn the_canonical_aws_account_ids_take_precedence_over_the_legacy_pair() {
        let _env = env_guard();
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
        let store = crate::secrets::FakeSecretStore::new();
        store.set("nova_access_key_id", "legacy-key").unwrap();
        store
            .set("nova_secret_access_key", "legacy-secret")
            .unwrap();
        store.set("aws_access_key_id", "current-key").unwrap();
        store
            .set("aws_secret_access_key", "current-secret")
            .unwrap();

        let (access_key_id, secret_access_key) = BedrockSection::default()
            .resolve_credentials(&store)
            .unwrap();
        assert_eq!(access_key_id, "current-key");
        assert_eq!(secret_access_key, "current-secret");
    }

    /// Both spellings must stay writable/deletable from the Settings UI —
    /// rejecting the legacy id would strand the entry that wrote it.
    #[test]
    fn both_the_canonical_and_legacy_account_ids_are_accepted() {
        assert!(crate::secrets::is_known_account("aws_access_key_id"));
        assert!(crate::secrets::is_known_account("aws_secret_access_key"));
        assert!(crate::secrets::is_known_account("nova_access_key_id"));
        assert!(crate::secrets::is_known_account("nova_secret_access_key"));
        assert!(!crate::secrets::is_known_account("bedrock_access_key_id"));
    }

    #[test]
    fn inline_nova_credentials_are_read_when_env_vars_are_absent() {
        let _env = env_guard();
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
        let nova = BedrockSection {
            access_key_id: Some("inline-key".into()),
            secret_access_key: Some("inline-secret".into()),
            ..Default::default()
        };
        let store = crate::secrets::FakeSecretStore::new();
        let (access_key_id, secret_access_key) = nova.resolve_credentials(&store).unwrap();
        assert_eq!(access_key_id, "inline-key");
        assert_eq!(secret_access_key, "inline-secret");
    }

    #[test]
    fn missing_nova_credentials_are_a_clear_config_error_not_a_panic() {
        let _env = env_guard();
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
        let nova = BedrockSection::default();
        let store = crate::secrets::FakeSecretStore::new();
        assert!(nova.resolve_credentials(&store).is_err());
    }

    #[test]
    fn a_nova_secret_access_key_never_reappears_in_a_debug_print_of_the_section() {
        let nova = BedrockSection {
            access_key_id: Some("AKIA-not-secret".into()),
            secret_access_key: Some("super-secret".into()),
            ..Default::default()
        };
        let rendered = format!("{nova:?}");
        assert!(
            !rendered.contains("super-secret"),
            "the raw secret must not leak through Debug: {rendered}"
        );
    }

    #[test]
    fn nova_key_file_is_read_and_parsed_when_inline_and_env_are_absent() {
        let _env = env_guard();
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
        let dir = std::env::temp_dir();
        let path = dir.join(format!("uia-test-nova-key-{}.key", std::process::id()));
        std::fs::write(
            &path,
            "access-key: 'AKIAEXAMPLE'\nsecret-access-key: 'example-secret+/=='\n",
        )
        .unwrap();
        let nova = BedrockSection {
            key_file: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let store = crate::secrets::FakeSecretStore::new();
        let result = nova.resolve_credentials(&store);
        std::fs::remove_file(&path).ok();
        let (access_key_id, secret_access_key) = result.unwrap();
        assert_eq!(access_key_id, "AKIAEXAMPLE");
        assert_eq!(secret_access_key, "example-secret+/==");
    }

    #[test]
    fn a_nova_key_file_missing_a_required_field_is_a_clear_config_error() {
        let _env = env_guard();
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
        let dir = std::env::temp_dir();
        let path = dir.join(format!("uia-test-nova-key-bad-{}.key", std::process::id()));
        std::fs::write(&path, "access-key: 'AKIAEXAMPLE'\n").unwrap();
        let nova = BedrockSection {
            key_file: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let store = crate::secrets::FakeSecretStore::new();
        let err = nova.resolve_credentials(&store).unwrap_err();
        std::fs::remove_file(&path).ok();
        assert!(err.to_string().contains("secret-access-key"), "got: {err}");
    }

    #[test]
    fn a_relative_nova_key_file_resolves_against_the_config_files_directory_not_cwd() {
        let _env = env_guard();
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
        let dir = std::env::temp_dir().join(format!("uia-nova-cfg-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("aws-api.key"),
            "access-key: 'AKIAEXAMPLE'\nsecret-access-key: 'example-secret'\n",
        )
        .unwrap();
        let toml_path = dir.join("uia.toml");
        std::fs::write(
            &toml_path,
            "[engine]\ndefault = \"bedrock\"\n[bedrock]\nkey_file = \"aws-api.key\"\n",
        )
        .unwrap();

        let cfg = Config::load(&toml_path).unwrap();
        let store = crate::secrets::FakeSecretStore::new();
        let result = cfg.bedrock.resolve_credentials(&store);

        std::fs::remove_dir_all(&dir).ok();
        let (access_key_id, secret_access_key) = result.unwrap();
        assert_eq!(access_key_id, "AKIAEXAMPLE");
        assert_eq!(secret_access_key, "example-secret");
    }

    #[test]
    fn find_config_file_walks_up_from_a_nested_start_directory() {
        let root = std::env::temp_dir().join(format!("uia-find-cfg-{}", std::process::id()));
        let nested = root.join("crates").join("uia-app");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join("uia.toml"), "").unwrap();

        let found = find_config_file(&nested, "uia.toml");

        std::fs::remove_dir_all(&root).ok();
        assert_eq!(found, Some(root.join("uia.toml")));
    }

    #[test]
    fn find_config_file_returns_none_when_no_ancestor_has_it() {
        let dir = std::env::temp_dir().join(format!("uia-find-cfg-miss-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let found = find_config_file(&dir, "uia-file-that-does-not-exist.toml");

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(found, None);
    }

    #[test]
    fn a_relative_key_file_resolves_against_the_config_files_directory_not_cwd() {
        let _env = env_guard();
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let dir = std::env::temp_dir().join(format!("uia-cfg-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("gpt-api.key"), "  from-relative-key-file\n").unwrap();
        let toml_path = dir.join("uia.toml");
        std::fs::write(&toml_path, "[openai]\nkey_file = \"gpt-api.key\"\n").unwrap();

        let cfg = Config::load(&toml_path).unwrap();
        let store = crate::secrets::FakeSecretStore::new();
        let result = cfg.openai.resolve_api_key(&store);

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(result.unwrap(), "from-relative-key-file");
    }

    #[test]
    fn the_resolved_key_never_reappears_in_a_debug_print_of_the_section() {
        let openai = OpenAiSection {
            api_key: Some("super-secret".into()),
            ..Default::default()
        };
        let rendered = format!("{openai:?}");
        assert!(
            !rendered.contains("super-secret"),
            "the raw key must not leak through Debug: {rendered}"
        );
    }

    #[test]
    fn keystore_value_wins_over_env_var_for_openai() {
        let _env = env_guard();
        unsafe { std::env::set_var("OPENAI_API_KEY", "from-env") };
        let store = crate::secrets::FakeSecretStore::new();
        store.set("openai_api_key", "from-keystore").unwrap();
        let openai = OpenAiSection::default();
        let result = openai.resolve_api_key(&store);
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        assert_eq!(result.unwrap(), "from-keystore");
    }

    #[test]
    fn keystore_value_wins_over_inline_toml_for_openai() {
        let _env = env_guard();
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let store = crate::secrets::FakeSecretStore::new();
        store.set("openai_api_key", "from-keystore").unwrap();
        let openai = OpenAiSection {
            api_key: Some("inline-key".into()),
            ..Default::default()
        };
        assert_eq!(openai.resolve_api_key(&store).unwrap(), "from-keystore");
    }

    #[test]
    fn absent_keystore_value_falls_through_to_env_var_for_openai() {
        let _env = env_guard();
        unsafe { std::env::set_var("OPENAI_API_KEY", "from-env") };
        let store = crate::secrets::FakeSecretStore::new();
        let openai = OpenAiSection::default();
        let result = openai.resolve_api_key(&store);
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        assert_eq!(result.unwrap(), "from-env");
    }

    #[test]
    fn keystore_pair_wins_over_env_vars_for_nova() {
        let _env = env_guard();
        unsafe {
            std::env::set_var("AWS_ACCESS_KEY_ID", "env-key");
            std::env::set_var("AWS_SECRET_ACCESS_KEY", "env-secret");
        }
        let store = crate::secrets::FakeSecretStore::new();
        store.set("nova_access_key_id", "keystore-key").unwrap();
        store
            .set("nova_secret_access_key", "keystore-secret")
            .unwrap();
        let nova = BedrockSection::default();
        let result = nova.resolve_credentials(&store);
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
        assert_eq!(
            result.unwrap(),
            ("keystore-key".to_string(), "keystore-secret".to_string())
        );
    }

    #[test]
    fn foundry_section_defaults_when_absent_from_toml() {
        let toml = r#"
            [openai]
            api_key = "test-key"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.foundry.api_key, None);
        assert_eq!(c.foundry.endpoint, None);
        assert_eq!(
            c.foundry.deployment, "gpt-realtime-2.1",
            "must default to the deployment verified live"
        );
    }

    #[test]
    fn foundry_deployment_can_be_pinned_via_config() {
        let toml = r#"
            [foundry]
            deployment = "gpt-realtime-mini"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.foundry.deployment, "gpt-realtime-mini");
    }

    #[test]
    fn bedrock_model_defaults_to_the_only_sonic_id_and_can_be_pinned() {
        let bare: Config = toml::from_str(
            "[openai]
api_key = \"k\"",
        )
        .unwrap();
        assert_eq!(
            bare.bedrock.model, "amazon.nova-2-sonic-v1:0",
            "must default to the one Sonic id Bedrock ships"
        );

        let pinned: Config = toml::from_str(
            r#"
            [bedrock]
            model = "amazon.nova-9-sonic-v1:0"
        "#,
        )
        .unwrap();
        assert_eq!(
            pinned.bedrock.model, "amazon.nova-9-sonic-v1:0",
            "a pinned id must survive load, same as [openai] model and              [foundry] deployment"
        );
    }

    #[test]
    fn foundry_keystore_value_wins_over_inline_toml() {
        let _env = env_guard();
        unsafe { std::env::remove_var("AZURE_AI_FOUNDRY_API_KEY") };
        let store = crate::secrets::FakeSecretStore::new();
        store.set("foundry_api_key", "from-keystore").unwrap();
        let foundry = FoundrySection {
            api_key: Some("inline-key".into()),
            ..Default::default()
        };
        assert_eq!(foundry.resolve_api_key(&store).unwrap(), "from-keystore");
    }

    #[test]
    fn foundry_missing_credentials_are_a_clear_config_error() {
        let _env = env_guard();
        unsafe { std::env::remove_var("AZURE_AI_FOUNDRY_API_KEY") };
        let store = crate::secrets::FakeSecretStore::new();
        let foundry = FoundrySection::default();
        assert!(foundry.resolve_api_key(&store).is_err());
    }

    #[test]
    fn a_foundry_key_file_with_key_and_endpoint_lines_parses_both() {
        let raw = "endpoint: 'https://my-resource.services.ai.azure.com'\nkey: 'abc123'\n";
        assert_eq!(
            parse_foundry_key_file(raw),
            (
                "abc123".to_string(),
                Some("https://my-resource.services.ai.azure.com".to_string())
            )
        );
    }

    #[test]
    fn a_bare_foundry_key_file_with_no_key_line_uses_the_whole_trimmed_file() {
        assert_eq!(
            parse_foundry_key_file("  sk-just-a-key  \n"),
            ("sk-just-a-key".to_string(), None)
        );
    }

    #[test]
    fn foundry_resolve_endpoint_prefers_the_inline_value_over_the_key_file() {
        let dir = std::env::temp_dir().join(format!(
            "uia-foundry-key-file-endpoint-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("foundry-api.key");
        std::fs::write(
            &path,
            "key: 'abc'\nendpoint: 'https://from-key-file.example.com'\n",
        )
        .unwrap();

        let foundry = FoundrySection {
            endpoint: Some("https://inline.example.com".into()),
            key_file: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let resolved = foundry.resolve_endpoint();

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(resolved, Some("https://inline.example.com".to_string()));
    }

    #[test]
    fn foundry_resolve_endpoint_falls_back_to_the_key_file_when_no_inline_value_is_set() {
        let dir = std::env::temp_dir().join(format!(
            "uia-foundry-key-file-endpoint-fallback-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("foundry-api.key");
        std::fs::write(
            &path,
            "key: 'abc'\nendpoint: 'https://from-key-file.example.com'\n",
        )
        .unwrap();

        let foundry = FoundrySection {
            key_file: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let resolved = foundry.resolve_endpoint();

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            resolved,
            Some("https://from-key-file.example.com".to_string())
        );
    }

    #[test]
    fn plaintext_openai_key_is_migrated_when_keystore_is_empty() {
        let _env = env_guard();
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let store = crate::secrets::FakeSecretStore::new();
        let mut config = Config::default();
        config.openai.api_key = Some("inline-key".into());
        migrate_plaintext_credentials_into_keystore(&config, &store);
        assert_eq!(store.get("openai_api_key"), Some("inline-key".to_string()));
    }

    #[test]
    fn existing_keystore_value_is_never_overwritten_by_migration() {
        let _env = env_guard();
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let store = crate::secrets::FakeSecretStore::new();
        store.set("openai_api_key", "already-there").unwrap();
        let mut config = Config::default();
        config.openai.api_key = Some("inline-key".into());
        migrate_plaintext_credentials_into_keystore(&config, &store);
        assert_eq!(
            store.get("openai_api_key"),
            Some("already-there".to_string())
        );
    }

    #[test]
    fn migration_is_a_noop_when_no_plaintext_credential_resolves() {
        let _env = env_guard();
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let store = crate::secrets::FakeSecretStore::new();
        let config = Config::default();
        migrate_plaintext_credentials_into_keystore(&config, &store);
        assert_eq!(store.get("openai_api_key"), None);
    }

    #[test]
    fn nova_credential_pair_is_migrated_together() {
        let _env = env_guard();
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
        let store = crate::secrets::FakeSecretStore::new();
        let mut config = Config::default();
        config.bedrock.access_key_id = Some("inline-id".into());
        config.bedrock.secret_access_key = Some("inline-secret".into());
        migrate_plaintext_credentials_into_keystore(&config, &store);
        assert_eq!(
            store.get("nova_access_key_id"),
            Some("inline-id".to_string())
        );
        assert_eq!(
            store.get("nova_secret_access_key"),
            Some("inline-secret".to_string())
        );
    }

    #[test]
    fn an_env_var_only_openai_credential_is_never_migrated_into_the_keystore() {
        // Regression test: migration must never promote an env-var-only
        // credential into the keystore, since the keystore is checked first
        // on every later launch and would then permanently shadow a
        // subsequent rotation of that env var with no diagnostic.
        let _env = env_guard();
        unsafe { std::env::set_var("OPENAI_API_KEY", "from-env") };
        let store = crate::secrets::FakeSecretStore::new();
        let config = Config::default();
        migrate_plaintext_credentials_into_keystore(&config, &store);
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        assert_eq!(
            store.get("openai_api_key"),
            None,
            "an env-var-only credential must not be promoted into the keystore"
        );
    }

    #[test]
    fn an_env_var_only_nova_credential_pair_is_never_migrated_into_the_keystore() {
        let _env = env_guard();
        unsafe {
            std::env::set_var("AWS_ACCESS_KEY_ID", "env-key");
            std::env::set_var("AWS_SECRET_ACCESS_KEY", "env-secret");
        }
        let store = crate::secrets::FakeSecretStore::new();
        let config = Config::default();
        migrate_plaintext_credentials_into_keystore(&config, &store);
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
        assert_eq!(store.get("nova_access_key_id"), None);
        assert_eq!(store.get("nova_secret_access_key"), None);
    }

    #[test]
    fn an_env_var_only_foundry_credential_is_never_migrated_into_the_keystore() {
        let _env = env_guard();
        unsafe { std::env::set_var("AZURE_AI_FOUNDRY_API_KEY", "from-env") };
        let store = crate::secrets::FakeSecretStore::new();
        let config = Config::default();
        migrate_plaintext_credentials_into_keystore(&config, &store);
        unsafe { std::env::remove_var("AZURE_AI_FOUNDRY_API_KEY") };
        assert_eq!(store.get("foundry_api_key"), None);
    }

    #[test]
    fn foundry_key_is_migrated_when_keystore_is_empty() {
        let _env = env_guard();
        unsafe { std::env::remove_var("AZURE_AI_FOUNDRY_API_KEY") };
        let store = crate::secrets::FakeSecretStore::new();
        let mut config = Config::default();
        config.foundry.api_key = Some("inline-key".into());
        migrate_plaintext_credentials_into_keystore(&config, &store);
        assert_eq!(store.get("foundry_api_key"), Some("inline-key".to_string()));
    }

    #[test]
    fn nova_pair_migration_is_skipped_when_only_one_half_is_already_present() {
        let _env = env_guard();
        unsafe {
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
        let store = crate::secrets::FakeSecretStore::new();
        store.set("nova_access_key_id", "existing").unwrap();
        let mut config = Config::default();
        config.bedrock.access_key_id = Some("inline-id".into());
        config.bedrock.secret_access_key = Some("inline-secret".into());
        migrate_plaintext_credentials_into_keystore(&config, &store);
        assert_eq!(
            store.get("nova_access_key_id"),
            Some("existing".to_string()),
            "the existing half must not be overwritten"
        );
        assert_eq!(
            store.get("nova_secret_access_key"),
            None,
            "migration must skip the pair entirely, never half-migrate it"
        );
    }

    #[test]
    fn foundry_api_key_never_reappears_in_a_debug_print_of_the_section() {
        let foundry = FoundrySection {
            api_key: Some("super-secret".into()),
            key_file: Some("path/to/file".into()),
            endpoint: Some("https://api.example.com".into()),
            ..Default::default()
        };
        let rendered = format!("{foundry:?}");
        assert!(
            !rendered.contains("super-secret"),
            "the raw secret must not leak through Debug: {rendered}"
        );
        assert!(
            rendered.contains("<redacted>"),
            "must show redacted marker: {rendered}"
        );
        assert!(
            rendered.contains("path/to/file"),
            "key_file must be visible"
        );
        assert!(
            rendered.contains("https://api.example.com"),
            "endpoint must be visible"
        );
    }
}

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// `Serialize` is for [`crate::settings`] (the persisted UI choice), not for
/// `Config` — `Config`'s other sections stay `Deserialize`-only on purpose
/// (S12/S18: never round-trip credentials to disk).
#[derive(Debug, Deserialize, Serialize, PartialEq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum EngineChoice {
    OpenAi,
    /// AWS Bedrock, named for the platform so it reads alongside `OpenAi` and
    /// `Foundry` rather than after the model family it happens to run
    /// (Nova Sonic). The `nova` alias is what keeps an existing
    /// `uia-engine.json` written before the rename loading unchanged —
    /// removing it would silently reset a Bedrock user to the default engine.
    #[serde(alias = "nova")]
    Bedrock,
    Foundry,
}

/// The wire string the `set_engine` IPC command receives from `App.svelte`
/// and the `uia://engine` event sends back — kept as its own `FromStr`
/// rather than routing through `serde_json` for a bare two-value string, so
/// the IPC command handler in `main.rs` (untestable in this sandbox) has as
/// little logic left in it as possible.
impl std::str::FromStr for EngineChoice {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "openai" => Ok(EngineChoice::OpenAi),
            "bedrock" => Ok(EngineChoice::Bedrock),
            // Legacy wire id, accepted for the same reason as the serde alias
            // above: a persisted engine choice must survive the rename.
            "nova" => Ok(EngineChoice::Bedrock),
            "foundry" => Ok(EngineChoice::Foundry),
            other => Err(format!(
                "unknown engine {other:?}, expected openai, bedrock, or foundry"
            )),
        }
    }
}

impl EngineChoice {
    pub fn as_wire(self) -> &'static str {
        match self {
            EngineChoice::OpenAi => "openai",
            EngineChoice::Bedrock => "bedrock",
            EngineChoice::Foundry => "foundry",
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "lowercase")]
pub enum MemoryBackend {
    /// No history at all. What SP1 shipped, and still the right choice for a
    /// shared machine or anyone who would rather nothing was written down.
    ///
    /// Note what it now costs: the app disconnects deliberately -- on an idle
    /// timeout, when Settings opens, and when rotating before a provider's
    /// session ceiling -- and with no memory each of those resumes with the
    /// assistant having forgotten the conversation.
    None,
    /// Transcripts kept in `uia-conversation.jsonl` beside `uia.toml`, and
    /// replayed into each new connection. The default, because without it a
    /// routine reconnect silently loses the conversation.
    ///
    /// Local only: a plain file next to the config, no service, nothing sent
    /// anywhere. Deleting the `uia-client` folder clears it.
    #[default]
    Local,
}

#[derive(Debug, Deserialize, Clone, Copy)]
pub struct EngineSection {
    #[serde(default = "default_engine_choice")]
    pub default: EngineChoice,
}
fn default_engine_choice() -> EngineChoice {
    EngineChoice::OpenAi
}
impl Default for EngineSection {
    fn default() -> Self {
        Self {
            default: default_engine_choice(),
        }
    }
}

#[derive(Deserialize, Default)]
pub struct OpenAiSection {
    /// How long one realtime session may last before it is rotated,
    /// in seconds. `0` disables rotation.
    ///
    /// OpenAI closes a Realtime session at 60 minutes with a `session_expired`
    /// error. The cap is on wall-clock session age, not idle time, so an
    /// active conversation hits it just as surely as an abandoned one.
    ///
    /// Rotation happens 30 seconds early, so the reconnect lands between
    /// utterances instead of the provider closing the stream mid-sentence.
    /// Exposed as a setting because these are provider policy and can
    /// change without warning.
    ///
    /// Source: https://platform.openai.com/docs/guides/realtime
    #[serde(default)]
    pub max_session_duration_secs: Option<u64>,

    /// Never serialised back out; present only so a config file (not the
    /// preferred path) can carry it. `resolve_api_key` is the read path.
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub key_file: Option<String>,
    #[serde(default = "default_openai_model")]
    pub model: String,
    #[serde(default = "default_true")]
    pub enable_audio_cache: bool,
}
fn default_openai_model() -> String {
    "gpt-realtime-2.1-mini".into()
}
fn default_true() -> bool {
    true
}

impl std::fmt::Debug for OpenAiSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiSection")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("key_file", &self.key_file)
            .field("model", &self.model)
            .field("enable_audio_cache", &self.enable_audio_cache)
            .finish()
    }
}

impl OpenAiSection {
    /// The OS keystore (`openai_api_key`) wins, then `OPENAI_API_KEY`, then an
    /// inline `api_key`, then `key_file`. None of them present is a clear
    /// config error, never a panic at connect time.
    ///
    /// The inline field was declared and documented in `uia.example.toml`
    /// from S12 but never actually read — a config that set it got
    /// "no OpenAI credential", which reads as a missing key rather than an
    /// ignored one. It sits below the env var so the documented override still
    /// wins, and above `key_file` because it is the more specific of the two.
    pub fn resolve_api_key(
        &self,
        store: &dyn crate::secrets::SecretStore,
    ) -> Result<String, ConfigError> {
        if let Some(key) = store.get("openai_api_key") {
            return Ok(key);
        }
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            return Ok(key);
        }
        if let Some(key) = &self.api_key {
            return Ok(key.trim().to_string());
        }
        if let Some(path) = &self.key_file {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| ConfigError::Invalid(format!("openai.key_file {path}: {e}")))?;
            return Ok(raw.trim().to_string());
        }
        Err(ConfigError::Invalid(
            "no OpenAI credential: set OPENAI_API_KEY or openai.key_file".into(),
        ))
    }

    /// Plaintext-only resolution used solely by migration: checks inline
    /// `api_key` then `key_file`, never the env var or the keystore itself.
    /// Migration must never promote an env-var-only credential into the
    /// keystore — the keystore is checked first on every later launch, so a
    /// migrated env var would permanently shadow a subsequent rotation of
    /// that env var with no diagnostic.
    fn resolve_plaintext_only(&self) -> Option<String> {
        if let Some(key) = &self.api_key {
            return Some(key.trim().to_string());
        }
        if let Some(path) = &self.key_file {
            return std::fs::read_to_string(path)
                .ok()
                .map(|raw| raw.trim().to_string());
        }
        None
    }
}

#[derive(Deserialize)]
pub struct BedrockSection {
    /// How long one realtime session may last before it is rotated,
    /// in seconds. `0` disables rotation.
    ///
    /// Bedrock's `InvokeModelWithBidirectionalStream` response stream stays open
    /// for **8 minutes**. By far the tightest of the three, and the reason
    /// this setting exists per provider rather than as one global value.
    ///
    /// Rotation happens 30 seconds early, so the reconnect lands between
    /// utterances instead of the provider closing the stream mid-sentence.
    /// Exposed as a setting because these are provider policy and can
    /// change without warning.
    ///
    /// Source: https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_InvokeModelWithBidirectionalStream.html
    #[serde(default)]
    pub max_session_duration_secs: Option<u64>,

    /// MUST be explicit. The ambient AWS profile resolves to ap-southeast-2,
    /// where Nova Sonic does not exist.
    #[serde(default = "default_bedrock_region")]
    pub region: String,
    /// The Bedrock model id, same tier and shape as `openai.model` and
    /// `foundry.deployment`. Bedrock ships exactly one Sonic model today, so
    /// the Settings UI offers a one-entry `<select>` rather than free text —
    /// the point is that the choice is *expressed* the same way as the other
    /// two engines, so a second Sonic id is a config edit and a one-line
    /// catalog addition instead of a code change.
    #[serde(default = "default_bedrock_model")]
    pub model: String,
    /// IAM access keys, not a Bedrock API key: `InvokeModelWithBidirectionalStream`
    /// is one of the operations Bedrock API keys explicitly cannot authenticate
    /// (AWS docs, api-keys-use.html), so SigV4 access keys are the only shape
    /// that works for Nova Sonic's streaming session.
    #[serde(default)]
    pub access_key_id: Option<String>,
    #[serde(default)]
    pub secret_access_key: Option<String>,
    /// Quick fix for local testing, mirroring `openai.key_file` — expects
    /// `access-key: '...'` / `secret-access-key: '...'` lines. Slated to be
    /// replaced by the real secret store (see `uia_core::creds`, "SP2
    /// replaces this with the encrypted store").
    #[serde(default)]
    pub key_file: Option<String>,
}
fn default_bedrock_region() -> String {
    "ap-northeast-1".into()
}
fn default_bedrock_model() -> String {
    // Mirrors `uia_nova::DEFAULT_MODEL_ID` as a literal rather than importing
    // it, the same way `default_openai_model` and `default_foundry_deployment`
    // duplicate theirs. Both literals are pinned by tests, so drift fails.
    "amazon.nova-2-sonic-v1:0".into()
}
/// Provider ceilings on a single realtime session, in seconds. Applied when
/// the config leaves `max_session_duration_secs` unset. These are provider
/// policy, checked against their documentation rather than assumed -- see each
/// field's doc comment for the source -- and can change without warning, which
/// is exactly why they are overridable.
pub const DEFAULT_OPENAI_MAX_SESSION_SECS: u64 = 3600;
pub const DEFAULT_BEDROCK_MAX_SESSION_SECS: u64 = 480;
pub const DEFAULT_FOUNDRY_MAX_SESSION_SECS: u64 = 1800;

/// `None` means no rotation: either the caller configured `0`, or no ceiling
/// is known for this provider. Zero is the documented opt-out rather than a
/// zero-length session.
#[cfg(test)]
mod session_duration_tests {
    use super::*;

    /// Every engine must resolve to *some* ceiling by default.
    ///
    /// This became load-bearing when `build_session` disabled the idle
    /// deadline: rotation is now the only thing standing between a long-lived
    /// session and the provider closing it on us, which arrives as an error
    /// mid-conversation rather than as a state the HUD can show. A `None` here
    /// used to be survivable because the five-minute idle drop would catch it.
    /// It no longer is.
    #[test]
    fn every_engine_has_a_default_ceiling() {
        for fallback in [
            DEFAULT_OPENAI_MAX_SESSION_SECS,
            DEFAULT_BEDROCK_MAX_SESSION_SECS,
            DEFAULT_FOUNDRY_MAX_SESSION_SECS,
        ] {
            assert_eq!(
                resolve_max_session_duration(None, fallback),
                Some(std::time::Duration::from_secs(fallback)),
            );
        }
    }

    /// The ceilings differ by almost an order of magnitude, which is the whole
    /// reason they are per-provider rather than one shared number. Pinned so a
    /// careless edit that collapses them fails here rather than by letting a
    /// Bedrock session die mid-sentence.
    #[test]
    fn the_ceilings_are_provider_specific() {
        assert_eq!(DEFAULT_OPENAI_MAX_SESSION_SECS, 3600);
        assert_eq!(DEFAULT_BEDROCK_MAX_SESSION_SECS, 480);
        assert_eq!(DEFAULT_FOUNDRY_MAX_SESSION_SECS, 1800);
    }

    /// The ordering itself, enforced at compile time rather than by a test --
    /// these are constants, so there is no reason to wait for a test run to
    /// learn that an edit collapsed them.
    const _: () = assert!(DEFAULT_BEDROCK_MAX_SESSION_SECS < DEFAULT_FOUNDRY_MAX_SESSION_SECS);
    const _: () = assert!(DEFAULT_FOUNDRY_MAX_SESSION_SECS < DEFAULT_OPENAI_MAX_SESSION_SECS);

    /// A configured value wins over the fallback -- the reason these are
    /// overridable is that provider policy can change without warning.
    #[test]
    fn a_configured_ceiling_overrides_the_default() {
        assert_eq!(
            resolve_max_session_duration(Some(90), DEFAULT_OPENAI_MAX_SESSION_SECS),
            Some(std::time::Duration::from_secs(90)),
        );
    }

    /// Zero is the documented opt-out, not a zero-length session.
    ///
    /// Worth knowing what it now means: with the idle deadline off, setting
    /// this to zero leaves a session with no self-imposed limit at all, and
    /// the first sign of trouble will be the provider hanging up. That is a
    /// legitimate choice for a shell that wants it, but it is no longer
    /// backstopped by anything.
    #[test]
    fn zero_opts_out_of_rotation_entirely() {
        assert_eq!(
            resolve_max_session_duration(Some(0), DEFAULT_OPENAI_MAX_SESSION_SECS),
            None,
        );
    }
}

pub fn resolve_max_session_duration(
    configured: Option<u64>,
    fallback: u64,
) -> Option<std::time::Duration> {
    match configured.unwrap_or(fallback) {
        0 => None,
        secs => Some(std::time::Duration::from_secs(secs)),
    }
}

impl Default for BedrockSection {
    fn default() -> Self {
        Self {
            max_session_duration_secs: None,
            region: default_bedrock_region(),
            model: default_bedrock_model(),
            access_key_id: None,
            secret_access_key: None,
            key_file: None,
        }
    }
}

impl std::fmt::Debug for BedrockSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BedrockSection")
            .field("region", &self.region)
            .field(
                "access_key_id",
                &self.access_key_id.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "secret_access_key",
                &self.secret_access_key.as_ref().map(|_| "<redacted>"),
            )
            .field("key_file", &self.key_file)
            .finish()
    }
}

impl BedrockSection {
    /// The OS keystore (`nova_access_key_id`/`nova_secret_access_key`) wins
    /// when both are present, then `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`
    /// — matching the standard AWS SDK env var names so a user who already
    /// has them set for the CLI doesn't need a second, app-specific pair.
    /// Falls back to the inline config fields, same precedence as
    /// `OpenAiSection::resolve_api_key`.
    pub fn resolve_credentials(
        &self,
        store: &dyn crate::secrets::SecretStore,
    ) -> Result<(String, String), ConfigError> {
        // Canonical ids first, then the pre-rename ones — an existing user's
        // AWS keys live in their OS keychain under `nova_*` and cannot be
        // rewritten behind their back, so the read has to look in both places.
        let read = |account: &str| {
            store.get(account).or_else(|| {
                crate::secrets::legacy_alias_for(account).and_then(|legacy| store.get(legacy))
            })
        };
        let keystore_pair = (read("aws_access_key_id"), read("aws_secret_access_key"));
        if let (Some(id), Some(secret)) = keystore_pair {
            return Ok((id, secret));
        }
        let env_pair = (
            std::env::var("AWS_ACCESS_KEY_ID").ok(),
            std::env::var("AWS_SECRET_ACCESS_KEY").ok(),
        );
        if let (Some(id), Some(secret)) = env_pair {
            return Ok((id, secret));
        }
        if let (Some(id), Some(secret)) = (&self.access_key_id, &self.secret_access_key) {
            return Ok((id.clone(), secret.clone()));
        }
        if let Some(path) = &self.key_file {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| ConfigError::Invalid(format!("nova.key_file {path}: {e}")))?;
            return parse_nova_key_file(&raw)
                .map_err(|e| ConfigError::Invalid(format!("nova.key_file {path}: {e}")));
        }
        Err(ConfigError::Invalid(
            "no Nova credential: set AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, nova.access_key_id/nova.secret_access_key, or nova.key_file".into(),
        ))
    }

    /// Plaintext-only resolution used solely by migration: checks the inline
    /// `access_key_id`/`secret_access_key` pair then `key_file`, never the
    /// standard AWS env vars or the keystore itself. Migration must never
    /// promote an env-var-only credential into the keystore — the keystore
    /// is checked first on every later launch, so a migrated env var would
    /// permanently shadow a subsequent rotation of that env var with no
    /// diagnostic.
    fn resolve_plaintext_only(&self) -> Option<(String, String)> {
        if let (Some(id), Some(secret)) = (&self.access_key_id, &self.secret_access_key) {
            return Some((id.clone(), secret.clone()));
        }
        if let Some(path) = &self.key_file {
            let raw = std::fs::read_to_string(path).ok()?;
            return parse_nova_key_file(&raw).ok();
        }
        None
    }
}

/// Parses the `key: 'value'` lines a Bedrock console access-key export uses.
/// Quotes (single or double) around the value are optional and stripped.
fn parse_nova_key_file(raw: &str) -> Result<(String, String), String> {
    let mut access_key_id = None;
    let mut secret_access_key = None;
    for line in raw.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['\'', '"']).to_string();
        match key.trim() {
            "access-key" => access_key_id = Some(value),
            "secret-access-key" => secret_access_key = Some(value),
            _ => {}
        }
    }
    match (access_key_id, secret_access_key) {
        (Some(id), Some(secret)) => Ok((id, secret)),
        (None, _) => Err("missing 'access-key' line".into()),
        (_, None) => Err("missing 'secret-access-key' line".into()),
    }
}

/// Same `key: 'value'`-per-line shape as [`parse_nova_key_file`], for a
/// Foundry `key_file` that carries both the API key and the endpoint - a real
/// Azure AI Foundry resource's connection info is naturally a (key, endpoint)
/// pair, the same way Nova's is a (access key, secret key) pair. `key` is
/// required; when no `key:` line is present at all, the whole trimmed file is
/// used as the key instead - preserving the simpler bare-key-file shape
/// `FoundrySection::resolve_api_key`'s doc comment already documented before
/// any real key_file exercised it.
fn parse_foundry_key_file(raw: &str) -> (String, Option<String>) {
    let mut key = None;
    let mut endpoint = None;
    for line in raw.lines() {
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['\'', '"']).to_string();
        match field.trim() {
            "key" => key = Some(value),
            "endpoint" => endpoint = Some(value),
            _ => {}
        }
    }
    let key = key.unwrap_or_else(|| raw.trim().to_string());
    (key, endpoint)
}

#[derive(Deserialize)]
pub struct FoundrySection {
    /// How long one realtime session may last before it is rotated,
    /// in seconds. `0` disables rotation.
    ///
    /// Azure's GPT Realtime caps a session at 30 minutes -- half OpenAI's own
    /// -- and, like the others, on session age rather than on idle time.
    ///
    ///
    /// Rotation happens 30 seconds early, so the reconnect lands between
    /// utterances instead of the provider closing the stream mid-sentence.
    /// Exposed as a setting because these are provider policy and can
    /// change without warning.
    ///
    /// Source: https://learn.microsoft.com/en-us/azure/foundry/openai/how-to/realtime-audio
    #[serde(default)]
    pub max_session_duration_secs: Option<u64>,

    pub api_key: Option<String>,
    pub key_file: Option<String>,
    /// Not a secret - no OS-keystore tier for this field.
    pub endpoint: Option<String>,
    /// The Azure deployment name (`session.model` on the wire - see
    /// `uia_foundry::engine`'s doc comment on why that field addresses a
    /// deployment, not a model family), not exposed via the OS keystore or
    /// the Settings UI for the same reason `openai.model`/`nova.region`
    /// aren't: it is a uia.toml-only choice, same tier as those two.
    #[serde(default = "default_foundry_deployment")]
    pub deployment: String,
}
fn default_foundry_deployment() -> String {
    // Verified live end to end (`foundry_satisfies_the_engine_contract`);
    // mirrors `uia_foundry::DEFAULT_DEPLOYMENT` as a literal rather than
    // depending on that crate from here, the same way `default_openai_model`
    // duplicates `uia_openai::DEFAULT_MODEL`'s value instead of importing it.
    "gpt-realtime-2.1".into()
}
impl Default for FoundrySection {
    fn default() -> Self {
        Self {
            max_session_duration_secs: None,
            api_key: None,
            key_file: None,
            endpoint: None,
            deployment: default_foundry_deployment(),
        }
    }
}

impl std::fmt::Debug for FoundrySection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FoundrySection")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("key_file", &self.key_file)
            .field("endpoint", &self.endpoint)
            .field("deployment", &self.deployment)
            .finish()
    }
}

impl FoundrySection {
    /// The OS keystore (`foundry_api_key`) wins, then `AZURE_AI_FOUNDRY_API_KEY`,
    /// then an inline `api_key`, then `key_file` — the identical four-tier
    /// precedence as [`OpenAiSection::resolve_api_key`], with inline checked
    /// before `key_file` because it is the more specific of the two.
    pub fn resolve_api_key(
        &self,
        store: &dyn crate::secrets::SecretStore,
    ) -> Result<String, ConfigError> {
        if let Some(key) = store.get("foundry_api_key") {
            return Ok(key);
        }
        if let Ok(key) = std::env::var("AZURE_AI_FOUNDRY_API_KEY") {
            return Ok(key);
        }
        if let Some(key) = &self.api_key {
            return Ok(key.trim().to_string());
        }
        if let Some(path) = &self.key_file {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| ConfigError::Invalid(format!("foundry key_file: {e}")))?;
            return Ok(parse_foundry_key_file(&raw).0);
        }
        Err(ConfigError::Invalid(
            "no Foundry API key found (keystore, AZURE_AI_FOUNDRY_API_KEY, key_file, or inline api_key)".into(),
        ))
    }

    /// `endpoint`'s inline value wins over anything embedded in `key_file` -
    /// an explicit `uia.toml` value is a deliberate override, `key_file`'s
    /// `endpoint:` line is same-file convenience for the common case where a
    /// resource's key and endpoint were copied from the Azure portal
    /// together. Unlike `resolve_api_key`, this has no keystore or env-var
    /// tier: `endpoint` is not a secret (see the field's own doc comment).
    pub fn resolve_endpoint(&self) -> Option<String> {
        if let Some(endpoint) = &self.endpoint {
            return Some(endpoint.clone());
        }
        let path = self.key_file.as_ref()?;
        let raw = std::fs::read_to_string(path).ok()?;
        parse_foundry_key_file(&raw).1
    }

    /// Plaintext-only resolution used solely by migration: checks inline
    /// `api_key` then `key_file`, never the env var or the keystore itself.
    /// Migration must never promote an env-var-only credential into the
    /// keystore — the keystore is checked first on every later launch, so a
    /// migrated env var would permanently shadow a subsequent rotation of
    /// that env var with no diagnostic. See `resolve_plaintext_only` on
    /// `OpenAiSection`/`BedrockSection` for the same reasoning.
    fn resolve_plaintext_only(&self) -> Option<String> {
        if let Some(key) = &self.api_key {
            return Some(key.trim().to_string());
        }
        if let Some(path) = &self.key_file {
            return std::fs::read_to_string(path)
                .ok()
                .map(|raw| raw.trim().to_string());
        }
        None
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct MemorySection {
    #[serde(default)]
    pub backend: MemoryBackend,
}

/// Audio devices and the OS echo-cancellation switch.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct AudioSection {
    /// Use the operating system's own echo cancellation (WASAPI's
    /// Communications category on Windows, Voice Processing IO on macOS).
    /// `false` opens plain devices with none. No effect on Linux, where echo
    /// cancellation is the `aec` Cargo feature instead.
    #[serde(default = "default_true")]
    pub aec_enabled: bool,
    /// Pins the input device to the first one whose name contains this
    /// (case-insensitive). `None` uses the OS default: Windows'
    /// `eCommunications` role, or macOS's default input (followed when it
    /// changes).
    ///
    /// Exists because that default is not always trustworthy: a real case
    /// found Windows silently holding the render communications role on a
    /// monitor's embedded HDMI audio device despite the user's actual
    /// speakers being marked default in Sound settings, with no visible
    /// conflict in the UI to explain it. This sidesteps trusting the role
    /// assignment at all rather than debugging why it drifted.
    #[serde(default)]
    pub input_device: Option<String>,
    /// Same override as `input_device`, for the output device.
    #[serde(default)]
    pub output_device: Option<String>,
    /// Overrides `Session`'s `DEFAULT_BARGE_IN_THRESHOLD` (an RMS level,
    /// never wired to any config surface before this — the doc comment on
    /// the constant has said "tune on real hardware" since S1's Task 28,
    /// and nothing ever did). A confirmed-live symptom this exists for: a
    /// ~1s gap between speaking over the assistant and its audio actually
    /// stopping, on hardware/mic gain where normal speech takes a moment to
    /// cross the untuned default. `None` keeps that default.
    #[serde(default)]
    pub barge_in_threshold: Option<f32>,
}
impl Default for AudioSection {
    fn default() -> Self {
        Self {
            aec_enabled: default_true(),
            input_device: None,
            output_device: None,
            barge_in_threshold: None,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub engine: EngineSection,
    #[serde(default)]
    pub openai: OpenAiSection,
    /// `[bedrock]` in `uia.toml`. The `nova` alias keeps an existing
    /// user's `[nova]` section parsing — without it, a rename would turn
    /// their configured region and credentials into silent defaults.
    #[serde(default, alias = "nova")]
    pub bedrock: BedrockSection,
    #[serde(default)]
    pub foundry: FoundrySection,
    #[serde(default)]
    pub memory: MemorySection,
    #[serde(default)]
    pub audio: AudioSection,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config not readable: {0}")]
    Io(#[from] std::io::Error),
    #[error("config not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("{0}")]
    Invalid(String),
}

/// Search `start` and each of its ancestors in turn for `filename`, the way
/// git locates `.git` or cargo locates the workspace `Cargo.toml`. Tauri's
/// CLI runs the built binary from the crate directory (`crates/uia-app`),
/// not the repo root where `uia.toml` is documented to live, so a bare
/// relative path fails silently no matter how the app is launched — this
/// search is what makes discovery independent of that launch-time cwd.
pub fn find_config_file(start: &Path, filename: &str) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(filename);
        if candidate.exists() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

/// Opportunistic, non-destructive: only writes when the keystore has nothing
/// for that account yet, and never touches the plaintext source (TOML/key
/// file). Called once per process start, right after `Config::load`.
pub fn migrate_plaintext_credentials_into_keystore(
    config: &Config,
    store: &dyn crate::secrets::SecretStore,
) {
    if store.get("openai_api_key").is_none()
        && let Some(key) = config.openai.resolve_plaintext_only()
        && let Err(e) = store.set("openai_api_key", &key)
    {
        eprintln!("warning: failed to migrate openai_api_key into OS keystore: {e}");
    }
    let nova_id_present = store.get("nova_access_key_id").is_some();
    let nova_secret_present = store.get("nova_secret_access_key").is_some();
    if !nova_id_present
        && !nova_secret_present
        && let Some((id, secret)) = config.bedrock.resolve_plaintext_only()
    {
        if let Err(e) = store.set("nova_access_key_id", &id) {
            eprintln!("warning: failed to migrate nova_access_key_id into OS keystore: {e}");
        }
        if let Err(e) = store.set("nova_secret_access_key", &secret) {
            eprintln!("warning: failed to migrate nova_secret_access_key into OS keystore: {e}");
        }
    }
    if store.get("foundry_api_key").is_none()
        && let Some(key) = config.foundry.resolve_plaintext_only()
        && let Err(e) = store.set("foundry_api_key", &key)
    {
        eprintln!("warning: failed to migrate foundry_api_key into OS keystore: {e}");
    }
}

/// Does this raw `uia.toml` still declare the removed `[mcp]` section?
///
/// A pure predicate rather than logic inlined into `load`, for the same
/// reason `session::mcp_targets` is split out from `build_executor`: the
/// decision is the part worth pinning down, and pinning it must not require
/// capturing stderr.
///
/// Commented-out lines do not count — `uia.example.toml` and `uia.toml` both
/// *describe* the removed section in prose, and a notice that fired on the
/// documentation telling you the section is gone would be worse than none.
/// TOML has no unknown-key rejection here, so an `[[mcp.servers]]` block
/// left behind by an upgrading user parses fine and is silently ignored;
/// this is what makes that silence audible.
fn mentions_removed_mcp_section(raw: &str) -> bool {
    raw.lines().any(|line| {
        let line = line.trim_start();
        if line.starts_with('#') {
            return false;
        }
        let table = line.trim_start_matches('[');
        table == "mcp]" || table.starts_with("mcp.")
    })
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path)?;
        // A notice, never an error: the section is ignored, not rejected, and
        // a user mid-upgrade must still start. But an MCP server that stops
        // working and says nothing is indistinguishable from a broken one —
        // the same reasoning behind `session::mcp_targets` logging a local
        // server it skips rather than dropping it quietly.
        if mentions_removed_mcp_section(&raw) {
            eprintln!(
                "warning: {} still has an [mcp] section; it is no longer read. MCP servers \
                 are now added in Settings -> MCP (install a .mcpb bundle, or add a remote \
                 HTTP server), and the section can be deleted.",
                path.display()
            );
        }
        let mut cfg: Config = toml::from_str(&raw)?;
        cfg.validate()?;
        // A relative key_file is written relative to uia.toml, not to
        // whatever cwd the process happens to run with — otherwise it is
        // just as launch-fragile as the uia.toml lookup above was.
        if let Some(key_file) = &cfg.openai.key_file {
            let key_path = Path::new(key_file);
            if key_path.is_relative()
                && let Some(base) = path.parent()
            {
                cfg.openai.key_file = Some(base.join(key_path).to_string_lossy().into_owned());
            }
        }
        if let Some(key_file) = &cfg.bedrock.key_file {
            let key_path = Path::new(key_file);
            if key_path.is_relative()
                && let Some(base) = path.parent()
            {
                cfg.bedrock.key_file = Some(base.join(key_path).to_string_lossy().into_owned());
            }
        }
        if let Some(key_file) = &cfg.foundry.key_file {
            let key_path = Path::new(key_file);
            if key_path.is_relative()
                && let Some(base) = path.parent()
            {
                cfg.foundry.key_file = Some(base.join(key_path).to_string_lossy().into_owned());
            }
        }
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        uia_nova::NovaEngine::validate_region(&self.bedrock.region)
            .map_err(|e| ConfigError::Invalid(e.to_string()))?;
        Ok(())
    }
}
