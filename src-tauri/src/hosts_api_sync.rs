//! Syncs the hosts-API source's machines into the manifest tree: one
//! single-choice folder of machine entries (so only one machine can be
//! on at a time — they serve the same domains, see `manifest.rs`'s
//! `folder_mode`), plus one always-present "company" entry outside it
//! (so it's included regardless of what's toggled on). This reuses the
//! existing exclusivity and apply/aggregate machinery as-is — nothing
//! about toggling or writing the system hosts file needed to change.
//!
//! Node ids for synced machines are derived deterministically from their
//! label (see `machine_node_id`), so a re-sync naturally finds and
//! updates the same node instead of needing a separate lookup field.
//! The folder and company-entry ids are fixed constants — there is only
//! ever one of each, so no id bookkeeping belongs in config.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, Runtime};

use crate::commands::{apply_aggregated_content, ApplyPipelineError};
use crate::hosts_api::{self, HostsApiError, Machine};
use crate::hosts_apply;
use crate::storage::{entries, manifest, manifest::Manifest, transaction, AppState};

pub const FOLDER_ID: &str = "hosts-api-folder";
pub const COMPANY_ENTRY_ID: &str = "hosts-api-company";
const FOLDER_TITLE: &str = "API servers";
const COMPANY_TITLE: &str = "Company (always on)";
const SYNC_INTERVAL: Duration = Duration::from_secs(60);
static FIRST_SYNC_DONE: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub enum SyncError {
    NotConfigured,
    NoPassword,
    Api(HostsApiError),
    Storage(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(
                f,
                "hosts API endpoint/username not set — save them first"
            ),
            Self::NoPassword => write!(f, "no saved password for this hosts API username"),
            Self::Api(e) => write!(f, "{e}"),
            Self::Storage(message) => write!(f, "{message}"),
        }
    }
}

impl From<HostsApiError> for SyncError {
    fn from(e: HostsApiError) -> Self {
        Self::Api(e)
    }
}

pub struct SyncOutcome {
    pub machines_synced: usize,
}

fn keyring_entry(username: &str) -> Result<keyring::Entry, SyncError> {
    keyring::Entry::new(hosts_api::KEYRING_SERVICE, username).map_err(|e| SyncError::Storage(e.to_string()))
}

pub fn store_password(username: &str, password: &str) -> Result<(), SyncError> {
    keyring_entry(username)?
        .set_password(password)
        .map_err(|e| SyncError::Storage(e.to_string()))
}

pub fn forget_password(username: &str) -> Result<(), SyncError> {
    match keyring_entry(username)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(SyncError::Storage(e.to_string())),
    }
}

fn read_password(username: &str) -> Result<String, SyncError> {
    keyring_entry(username)?
        .get_password()
        .map_err(|_| SyncError::NoPassword)
}

/// Fetches the current machine list + company block and syncs them into
/// the manifest. Network happens before any lock is taken (mirrors
/// `refresh.rs`'s "fetch outside `store_lock`" discipline — an HTTP
/// round trip must not block every other storage read/write).
pub async fn sync<R: Runtime>(app: &AppHandle<R>, state: &AppState) -> Result<SyncOutcome, SyncError> {
    let (base_url, username) = {
        let cfg = state.config.lock().expect("config mutex poisoned");
        (cfg.hosts_api_base_url.clone(), cfg.hosts_api_username.clone())
    };
    let base_url = base_url.filter(|s| !s.trim().is_empty()).ok_or(SyncError::NotConfigured)?;
    let username = username.filter(|s| !s.trim().is_empty()).ok_or(SyncError::NotConfigured)?;
    let password = read_password(&username)?;

    let client = hosts_api::build_client(state)?;
    let machines = hosts_api::fetch_machines(&client, &base_url, &username, &password).await?;
    let company = hosts_api::fetch_company_block(&client, &base_url, &username, &password).await?;
    let machines_synced = machines.len();

    // The first sync after startup re-applies even without a change, so a
    // system hosts file left stale by an earlier version gets corrected.
    let force_reapply = !FIRST_SYNC_DONE.swap(true, Ordering::SeqCst);
    let reapply_content = commit_sync(state, &machines, &company, force_reapply)?;

    // Reuses the same apply pipeline a manual Apply click uses (see
    // `apply_aggregated_content`'s doc comment); an unchanged file is a no-op.
    if let Some(content) = reapply_content {
        if let Err(ApplyPipelineError::Apply(e)) = apply_aggregated_content(app, state, &content).await {
            log::warn!("hosts API sync: failed to refresh the system hosts file: {e}");
        }
    }

    // Mirror the scanner's broadcast so List components rerun loadHostsData.
    let _ = app.emit("reload_list", json!({ "_args": [] }));

    Ok(SyncOutcome { machines_synced })
}

/// Wakes every 60s and re-syncs if a hosts-API source is configured, so
/// servers added/removed on the backend show up without the user having
/// to reopen the connect dialog. A failed attempt (no network, expired
/// session, endpoint unreachable, ...) is logged and nothing else --
/// never surfaced as a blocking error -- and just tried again next tick.
/// `sync()` itself no-ops cheaply (no network call) when nothing is
/// configured yet, so this is safe to always run.
pub fn start_background_sync<R: Runtime + 'static>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        loop {
            let state = app.state::<AppState>();
            match sync(&app, state.inner()).await {
                Ok(outcome) => {
                    log::info!("hosts API background sync: {} machine(s)", outcome.machines_synced)
                }
                Err(SyncError::NotConfigured) => {}
                Err(e) => log::warn!("hosts API background sync failed, retrying in 60s: {e}"),
            }
            tokio::time::sleep(SYNC_INTERVAL).await;
        }
    });
}

fn machine_node_id(label: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    label.hash(&mut hasher);
    format!("hosts-api-m-{:016x}", hasher.finish())
}

/// Returns `Some(aggregated_content)` when the live system hosts file
/// needs rewriting: an enabled entry was removed or its content changed,
/// or `force_reapply` is set and at least one API entry is enabled.
/// `None` otherwise. The content is computed here, while `manifest` and
/// `state.paths` are already in hand, but writing it is the caller's job:
/// it requires an `AppHandle` and a privileged write, neither of which
/// belong inside the store lock held for the rest of this function (see
/// A5 in `commands::apply_aggregated_content`'s doc comment).
fn commit_sync(
    state: &AppState,
    machines: &[Machine],
    company: &str,
    force_reapply: bool,
) -> Result<Option<String>, SyncError> {
    let _guard = state.lock_store().map_err(|e| SyncError::Storage(e.to_string()))?;
    let mut manifest = Manifest::load(&state.paths).map_err(|e| SyncError::Storage(e.to_string()))?;

    let mut touched_entries: Vec<String> = Vec::new();
    let mut needs_reapply = false;
    let mut any_api_entry_on = false;

    ensure_folder(&mut manifest.root);

    let wanted_ids: HashSet<String> = machines.iter().map(|m| machine_node_id(&m.label)).collect();
    let stale_ids: Vec<String> = manifest::find_node(&manifest.root, FOLDER_ID)
        .and_then(|folder| folder.get("children").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|child| child.get("id").and_then(Value::as_str).map(str::to_string))
        .filter(|id| !wanted_ids.contains(id))
        .collect();
    for id in stale_ids {
        if let Some((removed, _parent_id)) = manifest::remove_node(&mut manifest.root, &id) {
            if removed.get("on").and_then(Value::as_bool) == Some(true) {
                needs_reapply = true;
            }
        }
        entries::delete_entry(&state.paths.entries_dir, &id).map_err(|e| SyncError::Storage(e.to_string()))?;
        touched_entries.push(id);
    }

    for machine in machines {
        let id = machine_node_id(&machine.label);
        let updated = update_node_in_place(&mut manifest.root, &id, |node| {
            if let Some(obj) = node.as_object_mut() {
                obj.insert("title".into(), json!(machine.label));
                obj.insert("tags".into(), json!(machine.tags));
                obj.insert("hosts_api_state".into(), json!(machine.state));
                obj.insert("hosts_api_ip".into(), json!(machine.public_ip));
            }
        });
        if !updated {
            let node = json!({
                "id": id,
                "title": machine.label,
                "type": "local",
                "on": false,
                "tags": machine.tags,
                "hosts_api_label": machine.label,
                "hosts_api_state": machine.state,
                "hosts_api_ip": machine.public_ip,
            });
            manifest::insert_node(&mut manifest.root, node, Some(FOLDER_ID));
        }
        let content = machine.hosts_file.clone().unwrap_or_default();
        write_and_track(state, &manifest.root, &id, &content, &mut needs_reapply, &mut any_api_entry_on)?;
        touched_entries.push(id);
    }

    if manifest::find_node(&manifest.root, COMPANY_ENTRY_ID).is_none() {
        let node = json!({
            "id": COMPANY_ENTRY_ID,
            "title": COMPANY_TITLE,
            "type": "local",
            "on": true,
        });
        manifest::insert_node(&mut manifest.root, node, None);
    }
    write_and_track(
        state,
        &manifest.root,
        COMPANY_ENTRY_ID,
        company,
        &mut needs_reapply,
        &mut any_api_entry_on,
    )?;
    touched_entries.push(COMPANY_ENTRY_ID.to_string());

    let mut targets = vec![transaction::Target::Manifest];
    targets.extend(touched_entries.into_iter().map(transaction::Target::Entry));

    transaction::run(&state.paths, targets, || manifest.save(&state.paths))
        .map_err(|e| SyncError::Storage(e.to_string()))?;

    if !needs_reapply && !(force_reapply && any_api_entry_on) {
        return Ok(None);
    }

    let remove_duplicate_records = {
        let cfg = state.config.lock().expect("config mutex poisoned");
        cfg.remove_duplicate_records
    };
    let content = hosts_apply::aggregate_selected_content(&manifest.root, &state.paths, remove_duplicate_records)
        .map_err(|e| SyncError::Storage(e.to_string()))?;
    Ok(Some(content))
}

/// Writes an entry's new content, noting whether it is enabled and
/// whether an enabled entry's content actually changed (comparing with the
/// previous file is enough -- no separate hash needs storing).
fn write_and_track(
    state: &AppState,
    root: &[Value],
    id: &str,
    content: &str,
    needs_reapply: &mut bool,
    any_on: &mut bool,
) -> Result<(), SyncError> {
    let is_on = manifest::find_node(root, id)
        .and_then(|node| node.get("on").and_then(Value::as_bool))
        .unwrap_or(false);
    if is_on {
        *any_on = true;
        let previous = entries::read_entry(&state.paths.entries_dir, id).map_err(|e| SyncError::Storage(e.to_string()))?;
        if previous != content {
            *needs_reapply = true;
        }
    }
    entries::write_entry(&state.paths.entries_dir, id, content).map_err(|e| SyncError::Storage(e.to_string()))
}

fn ensure_folder(root: &mut Vec<Value>) {
    if manifest::find_node(root, FOLDER_ID).is_some() {
        return;
    }
    let folder = json!({
        "id": FOLDER_ID,
        "title": FOLDER_TITLE,
        "type": "folder",
        "folder_mode": 1,
        "children": [],
    });
    manifest::insert_node(root, folder, None);
}

/// Mutate a node's fields in place without disturbing anything the sync
/// doesn't own (`on`, `children`, position). Same recursive-walk shape
/// as `refresh.rs::stamp_refresh` — `manifest.rs` only exposes
/// find/remove/insert, not a generic in-place field update.
fn update_node_in_place(nodes: &mut [Value], id: &str, apply: impl FnMut(&mut Value)) -> bool {
    update_node_in_place_dyn(nodes, id, &mut { apply })
}

// A `&mut dyn FnMut` (rather than threading `impl FnMut` through the
// recursive call) keeps this a single concrete type across every
// recursion depth -- the generic version above recurses with a new
// `&mut &mut &mut ...` closure type each level and blows the compiler's
// monomorphization recursion limit on any sufficiently nested tree.
fn update_node_in_place_dyn(nodes: &mut [Value], id: &str, apply: &mut dyn FnMut(&mut Value)) -> bool {
    for node in nodes.iter_mut() {
        if node.get("id").and_then(Value::as_str) == Some(id) {
            apply(node);
            return true;
        }
        if let Some(children) = node.get_mut("children").and_then(Value::as_array_mut) {
            if update_node_in_place_dyn(children, id, apply) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_state() -> (AppState, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "switchhosts-hosts-api-sync-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let paths = crate::storage::V5Paths::under(root.clone());
        paths.ensure_dirs().unwrap();
        (
            AppState {
                paths,
                config: std::sync::Mutex::new(crate::storage::AppConfig::default()),
                store_lock: std::sync::Mutex::new(()),
                application_recovery: Default::default(),
                config_write_lock: std::sync::Mutex::new(()),
                is_will_quit: std::sync::atomic::AtomicBool::new(false),
                last_geometry_persist_ms: std::sync::atomic::AtomicU64::new(0),
                data_dir_recovery: None,
            },
            root,
        )
    }

    fn machine(label: &str, tags: &[&str]) -> Machine {
        Machine {
            label: label.to_string(),
            state: "running".to_string(),
            public_ip: Some("1.2.3.4".to_string()),
            hosts_file: Some(format!("1.2.3.4 {label}.test\n")),
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn first_sync_creates_a_single_choice_folder_with_machine_children() {
        let (state, root) = fixture_state();
        let _ = commit_sync(&state, &[machine("qa-1", &["qa"]), machine("qa-2", &[])], "1.1.1.1 company.test\n", false).unwrap();

        let manifest = Manifest::load(&state.paths).unwrap();
        let folder = manifest::find_node(&manifest.root, FOLDER_ID).unwrap();
        assert_eq!(folder["type"], "folder");
        assert_eq!(folder["folder_mode"], 1);
        assert_eq!(folder["children"].as_array().unwrap().len(), 2);

        let company = manifest::find_node(&manifest.root, COMPANY_ENTRY_ID).unwrap();
        assert_eq!(company["on"], true);
        assert_eq!(
            entries::read_entry(&state.paths.entries_dir, COMPANY_ENTRY_ID).unwrap(),
            "1.1.1.1 company.test\n"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resync_updates_existing_machine_without_resetting_on_state() {
        let (state, root) = fixture_state();
        let _ = commit_sync(&state, &[machine("qa-1", &["qa"])], "company\n", false).unwrap();

        let id = machine_node_id("qa-1");
        let mut manifest = Manifest::load(&state.paths).unwrap();
        update_node_in_place(&mut manifest.root, &id, |n| {
            n.as_object_mut().unwrap().insert("on".into(), json!(true));
        });
        manifest.save(&state.paths).unwrap();

        // Re-sync with an updated tag set and ip.
        let mut updated = machine("qa-1", &["qa", "new-tag"]);
        updated.public_ip = Some("5.6.7.8".to_string());
        let reapply = commit_sync(&state, &[updated], "company\n", false).unwrap();
        assert!(
            reapply.is_none(),
            "qa-1 is still returned by the API, just updated -- no reapply needed"
        );

        let manifest = Manifest::load(&state.paths).unwrap();
        let node = manifest::find_node(&manifest.root, &id).unwrap();
        assert_eq!(node["on"], true, "re-sync must not clobber the user's toggle");
        assert_eq!(node["tags"], json!(["qa", "new-tag"]));
        assert_eq!(node["hosts_api_ip"], "5.6.7.8");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resync_removes_machines_no_longer_returned_by_the_api() {
        let (state, root) = fixture_state();
        let _ = commit_sync(&state, &[machine("qa-1", &[]), machine("qa-2", &[])], "company\n", false).unwrap();
        let gone_id = machine_node_id("qa-2");

        let reapply = commit_sync(&state, &[machine("qa-1", &[])], "company\n", false).unwrap();
        assert!(
            reapply.is_none(),
            "qa-2 was never toggled on, so the system hosts file doesn't need rewriting"
        );

        let manifest = Manifest::load(&state.paths).unwrap();
        assert!(manifest::find_node(&manifest.root, &gone_id).is_none());
        assert!(!entries::entry_path(&state.paths.entries_dir, &gone_id)
            .unwrap()
            .exists());
        let folder = manifest::find_node(&manifest.root, FOLDER_ID).unwrap();
        assert_eq!(folder["children"].as_array().unwrap().len(), 1);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn removing_an_enabled_machine_reports_content_without_it() {
        // The critical case: qa-2 was toggled on (so its content is
        // live in the system hosts file right now) and then disappears
        // from the API. commit_sync must report fresh content with
        // qa-2 excluded, so the caller can rewrite the system file --
        // otherwise its entry lingers in /etc/hosts forever.
        let (state, root) = fixture_state();
        let _ = commit_sync(&state, &[machine("qa-1", &[]), machine("qa-2", &[])], "company\n", false).unwrap();

        let qa2_id = machine_node_id("qa-2");
        let mut manifest = Manifest::load(&state.paths).unwrap();
        update_node_in_place(&mut manifest.root, &qa2_id, |n| {
            n.as_object_mut().unwrap().insert("on".into(), json!(true));
        });
        manifest.save(&state.paths).unwrap();

        let reapply = commit_sync(&state, &[machine("qa-1", &[])], "company\n", false).unwrap();

        let content = reapply.expect("removing an enabled machine must trigger a reapply");
        assert!(
            !content.contains("qa-2.test"),
            "removed machine's content must not survive into the reapplied hosts file: {content}"
        );
        assert!(
            content.contains("company"),
            "the always-on company block must still be included: {content}"
        );
        assert!(
            !content.contains("qa-1.test"),
            "qa-1 was never toggled on, so it must not appear either: {content}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn removing_a_disabled_machine_does_not_report_content() {
        let (state, root) = fixture_state();
        let _ = commit_sync(&state, &[machine("qa-1", &[]), machine("qa-2", &[])], "company\n", false).unwrap();

        let reapply = commit_sync(&state, &[machine("qa-1", &[])], "company\n", false).unwrap();
        assert!(reapply.is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    fn turn_on(state: &AppState, id: &str) {
        let mut manifest = Manifest::load(&state.paths).unwrap();
        update_node_in_place(&mut manifest.root, id, |n| {
            n.as_object_mut().unwrap().insert("on".into(), json!(true));
        });
        manifest.save(&state.paths).unwrap();
    }

    fn with_hosts(label: &str, hosts: &str) -> Machine {
        let mut m = machine(label, &[]);
        m.hosts_file = Some(hosts.to_string());
        m
    }

    #[test]
    fn changed_content_of_an_enabled_machine_reports_the_new_content() {
        let (state, root) = fixture_state();
        let _ = commit_sync(&state, &[with_hosts("dev-1", "54.1.1.1 dev-1\n")], "company\n", false).unwrap();
        turn_on(&state, &machine_node_id("dev-1"));

        let reapply = commit_sync(&state, &[with_hosts("dev-1", "18.184.54.249 dev-1\n")], "company\n", false).unwrap();

        let content = reapply.expect("an enabled machine's new IP must reach the system hosts file");
        assert!(content.contains("18.184.54.249 dev-1"), "{content}");
        assert!(!content.contains("54.1.1.1"), "{content}");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn changed_content_of_a_disabled_machine_does_not_report_content() {
        let (state, root) = fixture_state();
        let _ = commit_sync(&state, &[with_hosts("dev-1", "54.1.1.1 dev-1\n")], "company\n", false).unwrap();

        let reapply = commit_sync(&state, &[with_hosts("dev-1", "18.184.54.249 dev-1\n")], "company\n", false).unwrap();
        assert!(reapply.is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn changed_company_block_reports_content() {
        let (state, root) = fixture_state();
        let _ = commit_sync(&state, &[], "1.1.1.1 old.company\n", false).unwrap();

        let content = commit_sync(&state, &[], "2.2.2.2 new.company\n", false)
            .unwrap()
            .expect("the always-on company block changed");
        assert!(content.contains("2.2.2.2 new.company"), "{content}");
        assert!(!content.contains("old.company"), "{content}");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unchanged_sync_reports_content_only_when_forced() {
        let (state, root) = fixture_state();
        let _ = commit_sync(&state, &[with_hosts("dev-1", "18.184.54.249 dev-1\n")], "company\n", false).unwrap();
        turn_on(&state, &machine_node_id("dev-1"));

        let unforced = commit_sync(&state, &[with_hosts("dev-1", "18.184.54.249 dev-1\n")], "company\n", false).unwrap();
        assert!(unforced.is_none(), "nothing changed, so no rewrite");

        let forced = commit_sync(&state, &[with_hosts("dev-1", "18.184.54.249 dev-1\n")], "company\n", true).unwrap();
        let content = forced.expect("first sync after startup must re-apply enabled entries");
        assert!(content.contains("18.184.54.249 dev-1"), "{content}");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn same_label_produces_a_stable_id_across_calls() {
        assert_eq!(machine_node_id("qa-1"), machine_node_id("qa-1"));
        assert_ne!(machine_node_id("qa-1"), machine_node_id("qa-2"));
    }

    #[test]
    fn store_and_forget_password_round_trip() {
        // Exercises the real OS keyring -- meaningful on a developer
        // machine (Keychain/Credential Manager/Secret Service are all
        // present), but a bare CI runner (e.g. ubuntu-latest with no
        // D-Bus Secret Service daemon) has no backend at all. That's an
        // environment limitation, not something this crate's thin
        // wrapper could be wrong about, so skip rather than fail when
        // there's no store to test against.
        let username = format!("sync-test-user-{}", std::process::id());
        if let Err(e) = store_password(&username, "s3cret") {
            eprintln!("skipping: no OS keyring backend available here ({e})");
            return;
        }
        assert_eq!(read_password(&username).unwrap(), "s3cret");
        forget_password(&username).unwrap();
        assert!(matches!(read_password(&username), Err(SyncError::NoPassword)));
    }
}
