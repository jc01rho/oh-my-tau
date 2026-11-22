//! Behavior tests for the layered settings model.

use omp_settings::{lookup, Provenance, ServiceTier, SettingType, Settings, SETTINGS};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omp-settings-test-{tag}-{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Env vars are process-global and cargo runs tests in parallel threads, so every
/// test that sets one holds this lock for its whole body.
static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

#[test]
fn schema_covers_ts_registry() {
    // 531 register({...}) definitions extracted from TS v18.6.1, plus the 19
    // settings only the TS panel (`createSettingsHost`) lists, plus the omp-rs
    // gates `magicKeywords.ultracode`, `systemPrompt.compact` and
    // `sandbox.enabled`.
    assert_eq!(SETTINGS.len(), 553);
    assert!(lookup("magicKeywords.ultracode").is_some());
    assert!(lookup("sandbox.enabled").is_some());
    assert!(lookup("systemPrompt.compact").is_some());
    assert!(lookup("modelRoles").is_some());
    assert!(lookup("cycleOrder").is_some());
    assert!(lookup("workspace.additionalDirectories").is_some());
    assert!(lookup("hindsight.globalScope").is_some());
    assert!(lookup("mnemopi.embeddingInputType").is_some());
    assert!(lookup("mnemopi.embeddingQueryPrefix").is_some());
    assert!(lookup("mnemopi.recallRelevance").is_some());
    assert!(lookup("sharpshooter.minPromptLength").is_some());
    assert!(lookup("nope.unknown").is_none());
    let tier = lookup("tier.openai").unwrap();
    assert!(matches!(tier.ty, SettingType::Enum(vals) if vals.contains(&"flex")));
}

#[test]
fn defaults_come_from_ts() {
    let dir = tempdir("defaults");
    let settings = Settings::load_from(None, &dir);
    assert_eq!(
        settings.cycle_order(),
        vec![
            "smol".to_string(),
            "default".to_string(),
            "slow".to_string()
        ]
    );
    assert_eq!(
        settings.stream_server_url().as_deref(),
        Some("https://live.omp.sh")
    );
    assert_eq!(settings.tier("openai"), Some(ServiceTier::None));
    assert_eq!(settings.tier("subagent"), Some(ServiceTier::Inherit));
    assert!(settings.disabled_extensions().is_empty());
    assert!(settings.disabled_providers().is_empty());
    assert!(settings.workspace_additional_directories().is_empty());
    assert!(!settings.images_urls_enabled());
    assert_eq!(
        settings.images_urls_backends(),
        vec!["provider-files", "tailscale", "cloudflared", "litterbox"]
    );
    // defaultThinkingLevel TS default is "high".
    assert_eq!(
        settings.get_string("defaultThinkingLevel").as_deref(),
        Some("high")
    );
}

#[test]
fn project_overrides_global_and_unknown_keys_preserved() {
    let root = tempdir("layers");
    let global = root.join("config.yml");
    fs::write(
        &global,
        "
cycleOrder: [a, b]
modelRoles:
  default: big-model
  judge: judge-model
customUserKey:
  nested: 1
",
    )
    .unwrap();
    let project = root.join(".omp");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("config.yml"),
        "
modelRoles:
  default: null
  smol: small-model
stream:
  serverUrl: http://localhost:9999
",
    )
    .unwrap();

    let settings = Settings::load_from(Some(&global), &project);
    assert_eq!(
        settings.cycle_order(),
        vec!["a".to_string(), "b".to_string()]
    );
    // null project role falls back to global; new project role merges in.
    let roles = settings.model_roles();
    assert_eq!(roles.get("default").map(String::as_str), Some("big-model"));
    assert_eq!(roles.get("judge").map(String::as_str), Some("judge-model"));
    assert_eq!(roles.get("smol").map(String::as_str), Some("small-model"));
    assert_eq!(
        settings.stream_server_url().as_deref(),
        Some("http://localhost:9999")
    );
    // Unknown keys are kept in the raw map, but only unknown paths are flagged.
    assert_eq!(
        settings.raw()["customUserKey"]["nested"],
        serde_json::json!(1)
    );
    assert_eq!(settings.unknown_keys(), vec!["customUserKey.nested"]);
}

#[test]
fn env_overrides_every_layer() {
    let _guard = lock_env();
    let dir = tempdir("env");
    // skillshare/client.ts documents OMP_AUTH_BROKER_TOKEN etc.; use a string env
    // setting and a boolean one with parseFlag semantics.
    std::env::set_var("OMP_AUTH_BROKER_URL", "https://broker.example");
    std::env::set_var("PI_AUTO_QA", "yes");
    let settings = Settings::load_from(None, &dir);
    assert_eq!(
        settings.get_string("auth.broker.url").as_deref(),
        Some("https://broker.example")
    );
    assert_eq!(settings.get_bool("dev.autoqa"), Some(true));
    std::env::remove_var("OMP_AUTH_BROKER_URL");
    std::env::remove_var("PI_AUTO_QA");
    assert_eq!(
        settings.provenance("auth.broker.url"),
        Some(Provenance::Env)
    );
    assert_eq!(settings.provenance("dev.autoqa"), Some(Provenance::Env));
    assert!(settings.global_path().is_none());
}

#[test]
fn provenance_distinguishes_all_four_layers() {
    let _guard = lock_env();
    let root = tempdir("provenance-layers");
    let global = root.join("config.yml");
    fs::write(
        &global,
        "stream:\n  serverUrl: http://global.example\ncycleOrder: [a, b]\n",
    )
    .unwrap();
    let project = root.join(".omp");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("config.yml"),
        "compaction:\n  enabled: true\nstream:\n  serverUrl: http://project.example\n",
    )
    .unwrap();

    std::env::remove_var("OMP_AUTH_BROKER_URL");
    std::env::set_var("OMP_AUTH_BROKER_URL", "https://env.example");
    let settings = Settings::load_from(Some(&global), &project);

    // Env beats both files.
    assert_eq!(
        settings.provenance("auth.broker.url"),
        Some(Provenance::Env)
    );
    assert_eq!(
        settings.get_string("auth.broker.url").as_deref(),
        Some("https://env.example")
    );
    // Project file beats the global file for the same id.
    assert_eq!(
        settings.provenance("stream.serverUrl"),
        Some(Provenance::Project)
    );
    assert_eq!(
        settings.stream_server_url().as_deref(),
        Some("http://project.example")
    );
    // Project-only id.
    assert_eq!(
        settings.provenance("compaction.enabled"),
        Some(Provenance::Project)
    );
    // Global supplies the value; the merged map holds it but the project is silent.
    assert_eq!(settings.provenance("cycleOrder"), Some(Provenance::Global));
    assert_eq!(
        settings.cycle_order(),
        vec!["a".to_string(), "b".to_string()]
    );
    // Registry default only: present in the merged map, still not "project".
    assert_eq!(
        settings.provenance("defaultThinkingLevel"),
        Some(Provenance::Default)
    );
    assert_eq!(
        settings.get_string("defaultThinkingLevel").as_deref(),
        Some("high")
    );
    // Registered setting with no default and no layer: unset, no provenance.
    assert_eq!(settings.provenance("browser.cdpUrl"), None);
    assert!(settings.get("browser.cdpUrl").is_null());
    // Not a registered setting at all.
    assert_eq!(settings.provenance("nope.unknown"), None);
    std::env::remove_var("OMP_AUTH_BROKER_URL");
}

#[test]
fn provenance_reports_default_for_untouched_settings() {
    let dir = tempdir("provenance-defaults");
    let settings = Settings::load_from(None, &dir);
    assert_eq!(settings.provenance("cycleOrder"), Some(Provenance::Default));
    assert_eq!(settings.provenance("modelRoles"), Some(Provenance::Default));
    assert_eq!(
        settings.provenance("stream.serverUrl"),
        Some(Provenance::Default)
    );

    // A global file that exists but does not mention the key changes nothing for it.
    let agent = dir.join("agent");
    fs::create_dir_all(&agent).unwrap();
    let global = agent.join("config.yml");
    fs::write(&global, "colorBlindMode: true\n").unwrap();
    let settings = Settings::load_from(Some(&global), &dir.join(".omp"));
    assert_eq!(settings.provenance("cycleOrder"), Some(Provenance::Default));
    assert_eq!(
        settings.provenance("colorBlindMode"),
        Some(Provenance::Global)
    );
    assert_eq!(settings.get_bool("colorBlindMode"), Some(true));
}

#[test]
fn provenance_record_fallback_keeps_lower_layer() {
    let root = tempdir("provenance-records");
    let global = root.join("config.yml");
    fs::write(&global, "modelRoles:\n  default: big-model\n").unwrap();

    // Project nulls the global role (TS fall-back) and adds nothing else: global wins.
    let null_only = root.join("null-only");
    fs::create_dir_all(&null_only).unwrap();
    fs::write(
        null_only.join("config.yml"),
        "modelRoles:\n  default: null\n",
    )
    .unwrap();
    let settings = Settings::load_from(Some(&global), &null_only);
    assert_eq!(settings.provenance("modelRoles"), Some(Provenance::Global));
    assert_eq!(
        settings.model_roles().get("default").map(String::as_str),
        Some("big-model")
    );

    // An empty record is the same fall-back marker.
    let empty = root.join("empty");
    fs::create_dir_all(&empty).unwrap();
    fs::write(empty.join("config.yml"), "modelRoles: {}\n").unwrap();
    let settings = Settings::load_from(Some(&global), &empty);
    assert_eq!(settings.provenance("modelRoles"), Some(Provenance::Global));

    // A project record that contributes a role takes over the layer.
    let contributing = root.join("contributing");
    fs::create_dir_all(&contributing).unwrap();
    fs::write(
        contributing.join("config.yml"),
        "modelRoles:\n  default: null\n  smol: small-model\n",
    )
    .unwrap();
    let settings = Settings::load_from(Some(&global), &contributing);
    assert_eq!(settings.provenance("modelRoles"), Some(Provenance::Project));
    assert_eq!(
        settings.model_roles().get("default").map(String::as_str),
        Some("big-model")
    );
    assert_eq!(
        settings.model_roles().get("smol").map(String::as_str),
        Some("small-model")
    );
}

#[test]
fn provenance_named_empty_preset_contributes() {
    let root = tempdir("provenance-presets");
    let global = root.join("config.yml");
    fs::write(&global, "modelPresets:\n  empty: {}\n").unwrap();

    // A named (but empty-bodied) preset is saved data, so the naming layer owns it.
    let project = root.join(".omp");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("config.yml"), "modelPresets:\n  other: {}\n").unwrap();
    let settings = Settings::load_from(Some(&global), &project);
    assert_eq!(
        settings.provenance("modelPresets"),
        Some(Provenance::Project)
    );
    assert_eq!(
        settings.raw()["modelPresets"]["other"],
        serde_json::json!({})
    );
    assert_eq!(
        settings.raw()["modelPresets"]["empty"],
        serde_json::json!({})
    );

    // A global named empty entry reports Global when the project stays silent.
    let settings = Settings::load_from(Some(&global), &root.join("no-project"));
    assert_eq!(
        settings.provenance("modelPresets"),
        Some(Provenance::Global)
    );

    // An unnamed empty record is still the fall-back marker: global keeps provenance.
    let blank = root.join("blank");
    fs::create_dir_all(&blank).unwrap();
    fs::write(blank.join("config.yml"), "modelPresets: {}\n").unwrap();
    let settings = Settings::load_from(Some(&global), &blank);
    assert_eq!(
        settings.provenance("modelPresets"),
        Some(Provenance::Global)
    );
}

#[test]
fn provenance_named_empty_tag_contributes() {
    let root = tempdir("provenance-tags");
    let global = root.join("config.yml");
    fs::write(&global, "modelTags:\n  empty: {}\n").unwrap();

    // A named (but empty-bodied) tag is saved data, so the naming layer owns it.
    let project = root.join(".omp");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("config.yml"), "modelTags:\n  other: {}\n").unwrap();
    let settings = Settings::load_from(Some(&global), &project);
    assert_eq!(settings.provenance("modelTags"), Some(Provenance::Project));
    assert_eq!(settings.raw()["modelTags"]["other"], serde_json::json!({}));
    assert_eq!(settings.raw()["modelTags"]["empty"], serde_json::json!({}));

    // A global named empty entry reports Global when the project stays silent.
    let settings = Settings::load_from(Some(&global), &root.join("no-project"));
    assert_eq!(settings.provenance("modelTags"), Some(Provenance::Global));

    // An unnamed empty record is still the fall-back marker: global keeps provenance.
    let blank = root.join("blank");
    fs::create_dir_all(&blank).unwrap();
    fs::write(blank.join("config.yml"), "modelTags: {}\n").unwrap();
    let settings = Settings::load_from(Some(&global), &blank);
    assert_eq!(settings.provenance("modelTags"), Some(Provenance::Global));
}

#[test]
fn provenance_env_requires_parseable_value() {
    let _guard = lock_env();
    let root = tempdir("provenance-env");
    let global = root.join("config.yml");
    fs::write(&global, "auth:\n  broker:\n    url: https://file.example\n").unwrap();
    let project = root.join(".omp");

    std::env::remove_var("OMP_AUTH_BROKER_URL");
    let settings = Settings::load_from(Some(&global), &project);
    assert_eq!(
        settings.provenance("auth.broker.url"),
        Some(Provenance::Global)
    );

    // Blank text is "unset" per parse_env_value, so the file layer still owns it.
    std::env::set_var("OMP_AUTH_BROKER_URL", "   ");
    let settings = Settings::load_from(Some(&global), &project);
    assert_eq!(
        settings.provenance("auth.broker.url"),
        Some(Provenance::Global)
    );
    assert_eq!(
        settings.get_string("auth.broker.url").as_deref(),
        Some("https://file.example")
    );

    std::env::set_var("OMP_AUTH_BROKER_URL", "https://env.example");
    let settings = Settings::load_from(Some(&global), &project);
    assert_eq!(
        settings.provenance("auth.broker.url"),
        Some(Provenance::Env)
    );

    std::env::remove_var("OMP_AUTH_BROKER_URL");
}

#[test]
fn model_presets_and_tags_parse() {
    let root = tempdir("presets");
    let global = root.join("config.yml");
    fs::write(
        &global,
        "
modelPresets:
  focused:
    modelRoles:
      default: glm
    defaultThinkingLevel: high
modelTags:
  cheap:
    models: [glm, qwen]
",
    )
    .unwrap();
    let settings = Settings::load_from(Some(&global), &root.join(".omp"));
    let presets = settings.model_presets();
    let preset = presets.get("focused").expect("preset parsed");
    assert_eq!(
        preset.model_roles.get("default").and_then(|v| v.as_str()),
        Some("glm")
    );
    assert_eq!(preset.default_thinking_level.as_deref(), Some("high"));
    let tags: BTreeMap<_, _> = settings.model_tags();
    assert_eq!(
        tags["cheap"].models,
        vec!["glm".to_string(), "qwen".to_string()]
    );
}

#[test]
fn real_user_config_loads_without_unknown_key_warnings() {
    // Read-only smoke against the actual user config TS already accepts.
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME set");
    let config = home.join(".omp").join("agent").join("config.yml");
    if !config.is_file() {
        eprintln!("skipping: no user config at {}", config.display());
        return;
    }
    let settings = Settings::load_from(Some(&config), &home.join(".omp"));
    assert!(
        settings.unknown_keys().is_empty(),
        "unknown keys TS knows: {:?}",
        settings.unknown_keys()
    );
    // A few known values from the real file (structure only).
    assert!(settings.get("modelRoles").is_object());
    assert!(settings.model_roles().contains_key("default"));
}
