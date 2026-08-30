use super::*;
use crate::ledger::conformance;

#[tokio::test]
async fn passes_the_conformance_suite() {
    let store = SqliteLedger::in_memory().expect("open in-memory ledger");
    conformance::run_all(&store).await;
}

#[tokio::test]
async fn passes_the_tenant_isolation_suite() {
    let store = SqliteLedger::in_memory().expect("open in-memory ledger");
    let a = store.for_tenant("user-a");
    let b = store.for_tenant("user-b");
    conformance::run_tenants(&store, &a, &b).await;
}

#[tokio::test]
async fn opening_a_legacy_episode_table_migrates_to_tenant_scoped_identity() {
    let root =
        std::env::temp_dir().join(format!("adaptive-episode-migration-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp dir");
    let path = root.join("ledger.db");
    let connection = Connection::open(&path).expect("legacy database");
    connection
        .execute_batch(
            "CREATE TABLE episodes (
                id TEXT PRIMARY KEY,
                scope_key TEXT NOT NULL DEFAULT '',
                goal TEXT NOT NULL,
                status TEXT NOT NULL,
                attempt INTEGER NOT NULL DEFAULT 0,
                stalled INTEGER NOT NULL DEFAULT 0,
                started_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
             );",
        )
        .expect("legacy schema");
    drop(connection);

    let store = SqliteLedger::open(&path).expect("migrate legacy schema");
    let a = store.for_tenant("user-a");
    let b = store.for_tenant("user-b");
    conformance::run_tenants(&store, &a, &b).await;

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_scoped_handle_shares_the_connection_rather_than_the_file() {
    // Two handles for the SAME tenant must see each other's writes — that
    // is what "shares" means. Probing it across scopes would now fail by
    // design, because rows carry the bucket that wrote them.
    let store = SqliteLedger::in_memory().expect("open in-memory ledger");
    let one = store.for_tenant("user-a");
    let two = store.for_tenant("user-a");
    one.append(&conformance::row("ep-shared", 1, "authored"))
        .await
        .expect("append");
    assert_eq!(two.rows("ep-shared").await.expect("rows").len(), 1);
    assert!(
        store.rows("ep-shared").await.expect("rows").is_empty(),
        "and the global bucket is its own, not a union"
    );
}

#[tokio::test]
async fn opening_a_path_creates_the_directory_holding_it() {
    // A first run against `/var/lib/whatever/ledger.db` must not fail
    // because nobody made the folder.
    let root = std::env::temp_dir().join(format!("adaptive-mkdir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let path = root.join("deep").join("nested").join("ledger.db");

    let store = SqliteLedger::open(&path).expect("open");
    store
        .append(&conformance::row("ep-mkdir", 1, "authored"))
        .await
        .expect("append");
    assert!(path.exists(), "{}", path.display());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_environment_moves_the_ledger_without_a_rebuild() {
    let fallback = std::path::Path::new("/srv/app/ledger.db");
    assert_eq!(
        chosen_path(Some("/mnt/data/ledger.db"), fallback),
        std::path::PathBuf::from("/mnt/data/ledger.db")
    );
}

#[test]
fn an_unset_environment_falls_back_to_the_path_in_the_code() {
    let fallback = std::path::Path::new("/srv/app/ledger.db");
    assert_eq!(chosen_path(None, fallback), fallback);
}

#[test]
fn a_blank_variable_reads_as_unset_rather_than_as_an_empty_path() {
    // What a shell leaves behind when a value was meant to be interpolated
    // and was not. Opening "" fails in a way that names nothing useful.
    let fallback = std::path::Path::new("/srv/app/ledger.db");
    assert_eq!(chosen_path(Some(""), fallback), fallback);
    assert_eq!(chosen_path(Some("   "), fallback), fallback);
}

fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move |key: &str| owned.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

#[test]
fn each_platform_uses_its_own_documented_directory() {
    let home = fake_env(&[("HOME", "/home/ada")]);
    assert_eq!(
        data_dir(Platform::Xdg, &home),
        Some("/home/ada/.local/share".into())
    );
    assert_eq!(
        data_dir(Platform::MacOs, &fake_env(&[("HOME", "/Users/ada")])),
        Some("/Users/ada/Library/Application Support".into())
    );
    assert_eq!(
        data_dir(
            Platform::Windows,
            &fake_env(&[("LOCALAPPDATA", "C:\\Users\\ada\\AppData\\Local")])
        ),
        Some("C:\\Users\\ada\\AppData\\Local".into())
    );
}

#[test]
fn xdg_data_home_wins_over_the_spec_s_own_fallback() {
    let env = fake_env(&[("XDG_DATA_HOME", "/data"), ("HOME", "/home/ada")]);
    assert_eq!(data_dir(Platform::Xdg, &env), Some("/data".into()));
}

#[test]
fn windows_uses_the_local_profile_not_the_roaming_one() {
    // A roaming profile syncs between machines, and a SQLite file copied
    // mid-write between two that both think they own it is a corrupted
    // database. Setting only APPDATA must therefore find nothing.
    let roaming = fake_env(&[("APPDATA", "C:\\Users\\ada\\AppData\\Roaming")]);
    assert_eq!(data_dir(Platform::Windows, &roaming), None);
}

#[test]
fn the_conventional_path_is_namespaced_by_project_and_named_for_the_crate() {
    let env = fake_env(&[("HOME", "/home/ada")]);
    assert_eq!(
        default_path(Platform::Xdg, &env).expect("path"),
        std::path::PathBuf::from("/home/ada/.local/share/tinyflows/adaptive.db")
    );
}

#[test]
fn the_variable_still_wins_over_the_convention() {
    let env = fake_env(&[(DB_PATH_VAR, "/mnt/data/ledger.db"), ("HOME", "/home/ada")]);
    assert_eq!(
        default_path(Platform::Xdg, &env).expect("path"),
        std::path::PathBuf::from("/mnt/data/ledger.db")
    );
}

#[test]
fn nowhere_conventional_is_an_error_that_says_what_to_set() {
    // A daemon under a user with no home. Guessing would put a database
    // somewhere nobody looks, and losing it silently is the failure this
    // whole crate is written to avoid.
    let err = default_path(Platform::Xdg, &fake_env(&[])).expect_err("no home");
    assert!(err.to_string().contains(DB_PATH_VAR), "{err}");
}

#[test]
fn a_configured_path_is_trimmed() {
    let fallback = std::path::Path::new("/srv/app/ledger.db");
    assert_eq!(
        chosen_path(Some("  /mnt/data/ledger.db\n"), fallback),
        std::path::PathBuf::from("/mnt/data/ledger.db")
    );
}

#[tokio::test]
async fn a_reopened_ledger_still_has_its_rows() {
    // The whole point of the sqlite backend over the in-memory one.
    let dir = std::env::temp_dir().join(format!("adaptive-ledger-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("ledger.db");
    let _ = std::fs::remove_file(&path);

    {
        let store = SqliteLedger::open(&path).expect("open");
        store
            .append(&conformance::row("ep", 1, "authored"))
            .await
            .expect("append");
    }
    let reopened = SqliteLedger::open(&path).expect("reopen");
    assert_eq!(reopened.rows("ep").await.expect("rows").len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn insertion_order_survives_a_timestamp_tie() {
    // Two attempts finishing in the same second is common; ordering by `at`
    // would make the exclusion list arbitrary.
    let store = SqliteLedger::in_memory().expect("open");
    for sig in ["first", "second", "third"] {
        let mut r = conformance::row("tie", 1, sig);
        r.at = "2026-01-01T00:00:00Z".to_string();
        store.append(&r).await.expect("append");
    }
    assert_eq!(
        store.tried("tie").await.expect("tried"),
        vec!["first", "second", "third"]
    );
}
