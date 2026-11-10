//! Discovery, validation, and lifecycle for installed IM extension providers.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::command::CancellationToken;
use crate::myna_settings::{MynaSettingsController, PersistenceRequest};
use crate::operation_gate::{BeginOperationError, OperationCoordinator, OperationKind};
use crate::ports::{ClientSettings, SystemConfigurator};
use myna_core::settings::KEY_INPUT_METHOD_EXTENSION;
use myna_platform::components::{
    Blocker, Component, ComponentError, ComponentStatus, Components, Purpose, StepKind,
};

pub const MANIFEST_VERSION: u32 = 1;
pub const MANIFEST_SUFFIX: &str = ".input-method.json";
pub const MANIFEST_DIRECTORY: &str = "/usr/share/myna/input-methods";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManifestCapabilities {
    pub preedit: bool,
    pub surrounding_text: bool,
    pub secure_field_detection: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderManifest {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub protocols: Vec<u32>,
    pub socket: String,
    pub capabilities: ManifestCapabilities,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestRecord {
    pub file_id: String,
    pub manifest: Result<ProviderManifest, String>,
}

pub trait ExtensionRegistry {
    fn records(&self) -> Result<Vec<ManifestRecord>, String>;
    fn socket_present(&self, id: &str) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionStatus {
    Enabled,
    Disabled,
    Unavailable,
    Broken,
}

impl ExtensionStatus {
    pub fn can_enable(self) -> bool {
        matches!(self, Self::Enabled | Self::Disabled)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionEntry {
    pub id: String,
    pub name: String,
    pub status: ExtensionStatus,
    pub selected: bool,
}

impl ExtensionEntry {
    /// A broken provider can still be switched off when it is the selection
    /// that is stopping dictation. A broken provider that is not selected is
    /// inert.
    pub fn can_toggle(&self) -> bool {
        self.status != ExtensionStatus::Broken || self.selected
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionSnapshot {
    pub entries: Vec<ExtensionEntry>,
    pub stale_active: Option<String>,
}

impl ExtensionSnapshot {
    pub fn active(&self) -> Option<&ExtensionEntry> {
        self.entries
            .iter()
            .find(|entry| entry.status == ExtensionStatus::Enabled)
    }

    /// The provider `active_id` selects. A selection whose manifest has
    /// disappeared is broken rather than absent: it must not silently make
    /// Myna fall back to IBus.
    pub fn active_provider(&self, active_id: Option<&str>) -> Option<ActiveProvider> {
        let id = active_id?;
        Some(
            self.entries
                .iter()
                .find(|entry| entry.id == id)
                .map(ActiveProvider::from)
                .unwrap_or_else(|| ActiveProvider::broken(id)),
        )
    }
}

/// The explicitly selected provider, as onboarding and Diagnostics describe
/// it. `ExtensionStatus` is the registry's existing vocabulary, not a second
/// provider-state model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveProvider {
    pub id: String,
    pub name: String,
    pub status: ExtensionStatus,
}

impl ActiveProvider {
    pub(crate) fn broken(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            name: id.to_owned(),
            status: ExtensionStatus::Broken,
        }
    }
}

impl From<&ExtensionEntry> for ActiveProvider {
    fn from(entry: &ExtensionEntry) -> Self {
        Self {
            id: entry.id.clone(),
            name: entry.name.clone(),
            status: entry.status,
        }
    }
}

pub struct ExtensionManager {
    registry: Rc<dyn ExtensionRegistry>,
}

impl ExtensionManager {
    pub fn new(registry: Rc<dyn ExtensionRegistry>) -> Self {
        Self { registry }
    }

    pub fn snapshot(&self, active_id: Option<&str>) -> Result<ExtensionSnapshot, String> {
        let records = self.registry.records()?;
        let mut entries = records
            .iter()
            .map(|record| {
                let (name, described_id, available) = match &record.manifest {
                    Ok(manifest) if record.file_id != manifest.id => (
                        record.file_id.clone(),
                        &record.file_id,
                        Err("manifest id does not match its file name".to_string()),
                    ),
                    Ok(manifest) if !safe_id(&manifest.id) => (
                        record.file_id.clone(),
                        &record.file_id,
                        Err("manifest id is not a safe provider id".to_string()),
                    ),
                    Ok(manifest)
                        if manifest.name.trim().is_empty()
                            || manifest.name.chars().count() > 128 =>
                    {
                        (
                            record.file_id.clone(),
                            &record.file_id,
                            Err("manifest name is invalid".to_string()),
                        )
                    }
                    Ok(manifest) if manifest.schema_version != MANIFEST_VERSION => (
                        manifest.name.clone(),
                        &manifest.id,
                        Err("manifest schema version is unsupported".to_string()),
                    ),
                    Ok(manifest) if !valid_protocols(&manifest.protocols) => (
                        manifest.name.clone(),
                        &manifest.id,
                        Err("manifest protocol versions are invalid".to_string()),
                    ),
                    Ok(manifest) if !manifest.protocols.contains(&1) => (
                        manifest.name.clone(),
                        &manifest.id,
                        Err("provider does not support protocol version 1".to_string()),
                    ),
                    Ok(manifest) if !manifest.capabilities.secure_field_detection => (
                        manifest.name.clone(),
                        &manifest.id,
                        Err("provider cannot detect secure fields".to_string()),
                    ),
                    Ok(manifest) if manifest.socket != format!("{}{}", manifest.id, ".sock") => (
                        manifest.name.clone(),
                        &manifest.id,
                        Err("provider socket name is invalid".to_string()),
                    ),
                    Ok(manifest) => (manifest.name.clone(), &manifest.id, Ok(())),
                    Err(_) => (
                        record.file_id.clone(),
                        &record.file_id,
                        Err("manifest is malformed".to_string()),
                    ),
                };
                let selected = active_id == Some(described_id.as_str());
                let status = match (
                    available,
                    selected,
                    self.registry.socket_present(described_id),
                ) {
                    (Err(_), _, _) => ExtensionStatus::Broken,
                    (Ok(()), true, true) => ExtensionStatus::Enabled,
                    (Ok(()), true, false) => ExtensionStatus::Unavailable,
                    (Ok(()), false, _) => ExtensionStatus::Disabled,
                };
                ExtensionEntry {
                    id: described_id.clone(),
                    name,
                    status,
                    selected,
                }
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });

        let stale_active = active_id
            .filter(|active| !entries.iter().any(|entry| entry.id == *active))
            .map(str::to_owned);
        Ok(ExtensionSnapshot {
            entries,
            stale_active,
        })
    }

    pub fn selection_allowed(
        &self,
        selected_id: Option<&str>,
        requested: &str,
    ) -> Result<bool, String> {
        Ok(self
            .snapshot(selected_id)?
            .entries
            .into_iter()
            .any(|entry| {
                entry.id == requested
                    && matches!(
                        entry.status,
                        ExtensionStatus::Enabled | ExtensionStatus::Disabled
                    )
            }))
    }

    pub fn active_provider(
        &self,
        active_id: Option<&str>,
    ) -> Result<Option<ActiveProvider>, String> {
        let Some(id) = active_id else {
            return Ok(None);
        };
        Ok(Some(
            self.snapshot(active_id)?
                .active_provider(Some(id))
                .expect("a non-empty active id has a provider"),
        ))
    }
}

/// A desktop's components with the active provider in place of its built-in
/// text-input component. With no active provider every call delegates
/// unchanged, so the existing IBus path remains the default.
pub struct InputMethodComponents {
    desktop: Rc<dyn Components>,
    manager: ExtensionManager,
    settings: Rc<dyn ClientSettings>,
}

impl InputMethodComponents {
    pub fn new(
        desktop: Rc<dyn Components>,
        registry: Rc<dyn ExtensionRegistry>,
        settings: Rc<dyn ClientSettings>,
    ) -> Self {
        Self {
            desktop,
            manager: ExtensionManager::new(registry),
            settings,
        }
    }

    fn active_provider(&self) -> Option<ActiveProvider> {
        let active = selected_provider(self.settings.as_ref()).ok().flatten()?;
        self.manager
            .active_provider(Some(&active))
            .unwrap_or_else(|_| Some(ActiveProvider::broken(&active)))
    }
}

#[async_trait::async_trait(?Send)]
impl Components for InputMethodComponents {
    fn required(&self) -> Vec<Component> {
        let Some(provider) = self.active_provider() else {
            return self.desktop.required();
        };
        let mut required = self.desktop.required();
        let provider_component = Component {
            id: provider.id,
            purpose: Purpose::TextInput,
        };
        if let Some(input) = required
            .iter_mut()
            .find(|component| component.purpose == Purpose::TextInput)
        {
            *input = provider_component;
        } else {
            let status_surface = required
                .iter()
                .position(|component| component.purpose == Purpose::StatusSurface)
                .unwrap_or(required.len());
            required.insert(status_surface, provider_component);
        }
        required
    }

    async fn status(&self, id: &str) -> ComponentStatus {
        let Some(provider) = self.active_provider().filter(|provider| provider.id == id) else {
            return self.desktop.status(id).await;
        };
        match provider.status {
            ExtensionStatus::Enabled => ComponentStatus::Active,
            ExtensionStatus::Disabled => ComponentStatus::Inactive,
            ExtensionStatus::Unavailable => ComponentStatus::Unavailable,
            ExtensionStatus::Broken => ComponentStatus::Blocked(Blocker::Incompatible),
        }
    }

    async fn enable(&self, id: &str) -> Result<(), ComponentError> {
        let Some(provider) = self.active_provider().filter(|provider| provider.id == id) else {
            return self.desktop.enable(id).await;
        };
        Err(ComponentError::Failed {
            kind: StepKind::Setting,
            step: KEY_INPUT_METHOD_EXTENSION.to_owned(),
            message: format!(
                "the input-method extension {} is managed in Myna Settings",
                provider.id
            ),
        })
    }
}

fn selected_provider(settings: &dyn ClientSettings) -> Result<Option<String>, String> {
    settings
        .get(KEY_INPUT_METHOD_EXTENSION)
        .map_err(|error| error.to_string())
        .map(|value| {
            value
                .as_str()
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionChangeCause {
    User,
    StaleSelection,
}

#[derive(Clone, Debug)]
pub struct ExtensionChange {
    request: PersistenceRequest,
    token: u64,
    cause: ExtensionChangeCause,
}

impl ExtensionChange {
    pub fn request(&self) -> PersistenceRequest {
        self.request.clone()
    }

    pub fn cause(&self) -> ExtensionChangeCause {
        self.cause
    }
}

#[derive(Clone, Debug)]
pub enum ExtensionSaveOutcome {
    Restart(ExtensionChange),
    Failed(ExtensionChangeCause),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionRestartOutcome {
    Succeeded,
    Failed(String),
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ExtensionActionError {
    #[error("another operation is already active")]
    Busy,
    #[error("this input-method extension cannot be enabled")]
    InvalidSelection,
    #[error("cannot discover input-method extensions: {0}")]
    Discovery(String),
    #[error("cannot save the input-method extension selection: {0}")]
    Save(String),
}

pub struct ExtensionLifecycle {
    manager: ExtensionManager,
    controller: Rc<MynaSettingsController>,
    operations: OperationCoordinator,
    busy: Cell<bool>,
    stale_clearing: Cell<bool>,
}

impl ExtensionLifecycle {
    pub fn new(
        registry: Rc<dyn ExtensionRegistry>,
        controller: Rc<MynaSettingsController>,
        operations: OperationCoordinator,
    ) -> Rc<Self> {
        Rc::new(Self {
            manager: ExtensionManager::new(registry),
            controller,
            operations,
            busy: Cell::new(false),
            stale_clearing: Cell::new(false),
        })
    }

    pub fn busy(&self) -> bool {
        self.busy.get() || self.stale_clearing.get()
    }

    pub fn controller(&self) -> Rc<MynaSettingsController> {
        Rc::clone(&self.controller)
    }

    pub fn snapshot(&self) -> Result<ExtensionSnapshot, ExtensionActionError> {
        let active = active_id(&self.controller);
        self.manager
            .snapshot(active.as_deref())
            .map_err(ExtensionActionError::Discovery)
    }

    pub fn active_provider(&self) -> Result<Option<ActiveProvider>, ExtensionActionError> {
        let active = active_id(&self.controller);
        self.manager
            .active_provider(active.as_deref())
            .map_err(ExtensionActionError::Discovery)
    }

    pub fn begin_user_change(
        &self,
        requested: Option<&str>,
    ) -> Result<Option<ExtensionChange>, ExtensionActionError> {
        if self.busy() {
            return Err(ExtensionActionError::Busy);
        }
        let active = active_id(&self.controller);
        if requested == active.as_deref() || (requested.is_none() && active.is_none()) {
            return Ok(None);
        }
        if let Some(id) = requested {
            match self.manager.selection_allowed(active.as_deref(), id) {
                Ok(true) => {}
                Ok(false) => return Err(ExtensionActionError::InvalidSelection),
                Err(error) => return Err(ExtensionActionError::Discovery(error)),
            }
        }
        let operation = self
            .operations
            .begin(OperationKind::ExtensionToggle)
            .map_err(|error| match error {
                BeginOperationError::Busy => ExtensionActionError::Busy,
            })?;
        self.busy.set(true);
        let request = match self.controller.set(
            KEY_INPUT_METHOD_EXTENSION,
            crate::domain::ClientSettingValue::Text(requested.unwrap_or_default().to_owned()),
        ) {
            Ok(request) => request,
            Err(error) => {
                self.operations.complete(operation.token());
                self.busy.set(false);
                return Err(ExtensionActionError::Save(error.to_string()));
            }
        };
        Ok(Some(ExtensionChange {
            request,
            token: operation.token(),
            cause: ExtensionChangeCause::User,
        }))
    }

    pub fn begin_stale_cleanup(&self) -> Result<Option<ExtensionChange>, ExtensionActionError> {
        if self.stale_clearing.replace(true) {
            return Ok(None);
        }
        let snapshot = match self.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.stale_clearing.set(false);
                return Err(error);
            }
        };
        if snapshot.stale_active.is_none() {
            self.stale_clearing.set(false);
            return Ok(None);
        }
        let operation = match self.operations.begin(OperationKind::ExtensionToggle) {
            Ok(operation) => operation,
            Err(BeginOperationError::Busy) => {
                self.stale_clearing.set(false);
                return Ok(None);
            }
        };
        let request = match self.controller.set(
            KEY_INPUT_METHOD_EXTENSION,
            crate::domain::ClientSettingValue::Text(String::new()),
        ) {
            Ok(request) => request,
            Err(error) => {
                self.operations.complete(operation.token());
                self.stale_clearing.set(false);
                return Err(ExtensionActionError::Save(error.to_string()));
            }
        };
        Ok(Some(ExtensionChange {
            request,
            token: operation.token(),
            cause: ExtensionChangeCause::StaleSelection,
        }))
    }

    pub fn save_finished(&self, change: ExtensionChange, saved: bool) -> ExtensionSaveOutcome {
        if saved {
            ExtensionSaveOutcome::Restart(change)
        } else {
            let cause = change.cause;
            self.finish(change);
            ExtensionSaveOutcome::Failed(cause)
        }
    }

    pub async fn restart(
        &self,
        change: ExtensionChange,
        configurator: &dyn SystemConfigurator,
    ) -> ExtensionRestartOutcome {
        let result = configurator.restart_myna(CancellationToken::new()).await;
        self.finish(change);
        match result {
            Ok(()) => ExtensionRestartOutcome::Succeeded,
            Err(error) => ExtensionRestartOutcome::Failed(error.to_string()),
        }
    }

    fn finish(&self, change: ExtensionChange) {
        let released = self.operations.complete(change.token);
        match change.cause {
            ExtensionChangeCause::User => self.busy.set(false),
            ExtensionChangeCause::StaleSelection => self.stale_clearing.set(false),
        }
        if !released {
            self.busy.set(false);
            self.stale_clearing.set(false);
        }
    }
}

pub struct SystemExtensionRegistry {
    manifest_dir: PathBuf,
    socket_dir: PathBuf,
}

impl SystemExtensionRegistry {
    pub fn new(manifest_dir: impl Into<PathBuf>, socket_dir: impl Into<PathBuf>) -> Self {
        Self {
            manifest_dir: manifest_dir.into(),
            socket_dir: socket_dir.into(),
        }
    }

    pub fn system() -> Self {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/run/user"));
        Self::new(
            MANIFEST_DIRECTORY,
            runtime.join("snap.myna").join("myna-input-methods"),
        )
    }
}

impl ExtensionRegistry for SystemExtensionRegistry {
    fn records(&self) -> Result<Vec<ManifestRecord>, String> {
        let directory = match fs::read_dir(&self.manifest_dir) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(format!(
                    "cannot read input-method manifests from {}: {error}",
                    self.manifest_dir.display()
                ));
            }
        };
        let mut records = Vec::new();
        for entry in directory {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(file_id) = file_name.strip_suffix(MANIFEST_SUFFIX) else {
                continue;
            };
            records.push(ManifestRecord {
                file_id: file_id.to_owned(),
                manifest: read_manifest(&path),
            });
        }
        records.sort_by(|left, right| left.file_id.cmp(&right.file_id));
        Ok(records)
    }

    fn socket_present(&self, id: &str) -> bool {
        safe_id(id)
            && fs::symlink_metadata(self.socket_dir.join(format!("{id}.sock")))
                .is_ok_and(|metadata| metadata.file_type().is_socket())
    }
}

/// The session's desktop components with the installed provider registry and
/// shared settings store attached. A failure to open the settings store keeps
/// the desktop's existing behaviour and is logged rather than guessed at.
pub fn provider_aware_components(desktop: Rc<dyn Components>) -> Rc<dyn Components> {
    let settings = match crate::adapters::client_settings::GioClientSettings::open() {
        Ok(settings) => Rc::new(settings) as Rc<dyn ClientSettings>,
        Err(error) => {
            gio::glib::g_warning!(
                crate::LOG_DOMAIN,
                "cannot read the active input-method provider: {error}"
            );
            return desktop;
        }
    };
    Rc::new(InputMethodComponents::new(
        desktop,
        Rc::new(SystemExtensionRegistry::system()),
        settings,
    ))
}

fn read_manifest(path: &Path) -> Result<ProviderManifest, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err("manifest exceeds the maximum size".to_string());
    }
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

fn valid_protocols(protocols: &[u32]) -> bool {
    let mut seen = BTreeSet::new();
    protocols
        .iter()
        .all(|version| *version > 0 && seen.insert(*version))
}

pub fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        && id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && id.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric)
}

pub fn active_id(settings: &crate::myna_settings::MynaSettingsController) -> Option<String> {
    settings
        .shown_value(KEY_INPUT_METHOD_EXTENSION)
        .and_then(|value| value.as_str().map(str::to_owned))
        .filter(|id| !id.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct FakeRegistry {
        records: RefCell<Vec<ManifestRecord>>,
        sockets: RefCell<Vec<String>>,
    }

    impl ExtensionRegistry for FakeRegistry {
        fn records(&self) -> Result<Vec<ManifestRecord>, String> {
            Ok(self.records.borrow().clone())
        }

        fn socket_present(&self, id: &str) -> bool {
            self.sockets.borrow().iter().any(|socket| socket == id)
        }
    }

    fn manifest(id: &str) -> ProviderManifest {
        ProviderManifest {
            schema_version: 1,
            id: id.to_owned(),
            name: format!("Extension {id}"),
            protocols: vec![1],
            socket: format!("{id}.sock"),
            capabilities: ManifestCapabilities {
                preedit: true,
                surrounding_text: true,
                secure_field_detection: true,
            },
        }
    }

    fn registry(records: Vec<ManifestRecord>, sockets: &[&str]) -> Rc<dyn ExtensionRegistry> {
        Rc::new(FakeRegistry {
            records: RefCell::new(records),
            sockets: RefCell::new(sockets.iter().map(|id| (*id).to_owned()).collect()),
        })
    }

    #[test]
    fn installed_extensions_are_disabled_until_selected() {
        let registry = registry(
            vec![ManifestRecord {
                file_id: "alpha".into(),
                manifest: Ok(manifest("alpha")),
            }],
            &["alpha"],
        );
        let snapshot = ExtensionManager::new(Rc::clone(&registry))
            .snapshot(None)
            .unwrap();
        assert_eq!(snapshot.entries[0].status, ExtensionStatus::Disabled);
        assert!(!snapshot.entries[0].selected);
    }

    #[test]
    fn selected_extension_with_a_live_socket_is_enabled() {
        let registry = registry(
            vec![ManifestRecord {
                file_id: "alpha".into(),
                manifest: Ok(manifest("alpha")),
            }],
            &["alpha"],
        );
        let snapshot = ExtensionManager::new(registry)
            .snapshot(Some("alpha"))
            .unwrap();
        assert_eq!(snapshot.entries[0].status, ExtensionStatus::Enabled);
        assert!(snapshot.entries[0].selected);
    }

    #[test]
    fn selected_extension_without_a_socket_stays_selected_but_unavailable() {
        let registry = registry(
            vec![ManifestRecord {
                file_id: "alpha".into(),
                manifest: Ok(manifest("alpha")),
            }],
            &[],
        );
        let snapshot = ExtensionManager::new(registry)
            .snapshot(Some("alpha"))
            .unwrap();
        assert_eq!(snapshot.entries[0].status, ExtensionStatus::Unavailable);
        assert!(snapshot.entries[0].selected);
        assert!(snapshot.stale_active.is_none());
    }

    #[test]
    fn a_missing_manifest_reports_and_returns_the_stale_active_id() {
        let registry = registry(Vec::new(), &[]);
        let snapshot = ExtensionManager::new(registry)
            .snapshot(Some("missing"))
            .unwrap();
        assert_eq!(snapshot.stale_active.as_deref(), Some("missing"));
        assert_eq!(
            snapshot.active_provider(Some("missing")),
            Some(ActiveProvider {
                id: "missing".into(),
                name: "missing".into(),
                status: ExtensionStatus::Broken,
            })
        );
        assert!(snapshot.entries.is_empty());
    }

    #[test]
    fn malformed_or_insecure_manifests_are_broken_and_cannot_be_enabled() {
        let mut insecure = manifest("insecure");
        insecure.capabilities.secure_field_detection = false;
        let registry = registry(
            vec![
                ManifestRecord {
                    file_id: "bad".into(),
                    manifest: Err("bad json".into()),
                },
                ManifestRecord {
                    file_id: "insecure".into(),
                    manifest: Ok(insecure),
                },
            ],
            &["bad", "insecure"],
        );
        let snapshot = ExtensionManager::new(Rc::clone(&registry))
            .snapshot(None)
            .unwrap();
        assert!(snapshot.entries.iter().all(|entry| {
            entry.status == ExtensionStatus::Broken && !entry.status.can_enable()
        }));
        let manager = ExtensionManager::new(Rc::clone(&registry));
        assert!(!manager.selection_allowed(None, "insecure").unwrap());
    }

    #[test]
    fn duplicate_protocol_versions_and_unknown_capabilities_violate_the_schema() {
        let mut duplicate = manifest("duplicate");
        duplicate.protocols = vec![1, 1];
        let registry = registry(
            vec![ManifestRecord {
                file_id: "duplicate".into(),
                manifest: Ok(duplicate),
            }],
            &["duplicate"],
        );
        let snapshot = ExtensionManager::new(registry).snapshot(None).unwrap();
        assert_eq!(snapshot.entries[0].status, ExtensionStatus::Broken);

        let unknown_capability = serde_json::json!({
            "schema_version": 1,
            "id": "alpha",
            "name": "Alpha",
            "protocols": [1],
            "socket": "alpha.sock",
            "capabilities": {
                "preedit": true,
                "surrounding_text": true,
                "secure_field_detection": true,
                "exec": "anything"
            }
        });
        assert!(serde_json::from_value::<ProviderManifest>(unknown_capability).is_err());
    }

    #[test]
    fn every_manifest_field_is_validated_before_a_provider_is_usable() {
        let mut mismatched_file = manifest("alpha");
        mismatched_file.id = "alpha".into();
        let mut unsafe_id = manifest("alpha");
        unsafe_id.id = "../alpha".into();
        let mut empty_name = manifest("alpha");
        empty_name.name.clear();
        let mut long_name = manifest("alpha");
        long_name.name = "x".repeat(129);
        let mut wrong_schema = manifest("alpha");
        wrong_schema.schema_version = 2;
        let mut no_protocols = manifest("alpha");
        no_protocols.protocols.clear();
        let mut unsupported_protocol = manifest("alpha");
        unsupported_protocol.protocols = vec![2];
        let mut wrong_socket = manifest("alpha");
        wrong_socket.socket = "other.sock".into();
        let mut insecure = manifest("alpha");
        insecure.capabilities.secure_field_detection = false;

        for (file_id, manifest) in [
            ("other", mismatched_file),
            ("alpha", unsafe_id),
            ("alpha", empty_name),
            ("alpha", long_name),
            ("alpha", wrong_schema),
            ("alpha", no_protocols),
            ("alpha", unsupported_protocol),
            ("alpha", wrong_socket),
            ("alpha", insecure),
        ] {
            let snapshot = ExtensionManager::new(registry(
                vec![ManifestRecord {
                    file_id: file_id.into(),
                    manifest: Ok(manifest),
                }],
                &["alpha"],
            ))
            .snapshot(None)
            .unwrap();
            assert_eq!(snapshot.entries[0].status, ExtensionStatus::Broken);
        }
    }

    #[test]
    fn the_system_adapter_ignores_unrelated_files_and_reads_valid_manifests() {
        let directory = std::env::temp_dir().join(format!(
            "myna-extension-manifest-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let manifest = manifest("alpha");
        fs::write(
            directory.join("alpha.input-method.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(directory.join("README"), "not a manifest").unwrap();

        let registry = SystemExtensionRegistry::new(&directory, directory.join("runtime"));
        let records = registry.records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].manifest.as_ref().unwrap().id, "alpha");
        fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn a_selected_broken_extension_can_still_be_disabled() {
        let mut insecure = manifest("alpha");
        insecure.capabilities.secure_field_detection = false;
        let registry = registry(
            vec![ManifestRecord {
                file_id: "alpha".into(),
                manifest: Ok(insecure),
            }],
            &["alpha"],
        );
        let snapshot = ExtensionManager::new(registry)
            .snapshot(Some("alpha"))
            .unwrap();
        let entry = &snapshot.entries[0];
        assert_eq!(entry.status, ExtensionStatus::Broken);
        assert!(entry.selected);
        assert!(entry.can_toggle());
        assert!(snapshot.stale_active.is_none());
    }

    struct FakeDesktop {
        enabled: RefCell<Vec<String>>,
    }

    impl Default for FakeDesktop {
        fn default() -> Self {
            Self {
                enabled: RefCell::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait(?Send)]
    impl Components for FakeDesktop {
        fn required(&self) -> Vec<Component> {
            vec![
                Component {
                    id: "ibus".into(),
                    purpose: Purpose::TextInput,
                },
                Component {
                    id: "indicator".into(),
                    purpose: Purpose::StatusSurface,
                },
            ]
        }

        async fn status(&self, id: &str) -> ComponentStatus {
            if id == "ibus" {
                ComponentStatus::Active
            } else {
                ComponentStatus::Unavailable
            }
        }

        async fn enable(&self, id: &str) -> Result<(), ComponentError> {
            self.enabled.borrow_mut().push(id.to_owned());
            Ok(())
        }
    }

    fn provider_components(
        records: Vec<ManifestRecord>,
        sockets: &[&str],
        active: &str,
    ) -> (InputMethodComponents, Rc<FakeDesktop>) {
        let desktop = Rc::new(FakeDesktop::default());
        let (settings, _) = settings_controller(active);
        (
            InputMethodComponents::new(
                Rc::clone(&desktop) as Rc<dyn Components>,
                registry(records, sockets),
                Rc::clone(&settings) as Rc<dyn ClientSettings>,
            ),
            desktop,
        )
    }

    #[test]
    fn an_enabled_provider_replaces_the_desktops_ibus_component() {
        let (components, desktop) = provider_components(
            vec![ManifestRecord {
                file_id: "alpha".into(),
                manifest: Ok(manifest("alpha")),
            }],
            &["alpha"],
            "alpha",
        );
        assert_eq!(
            components.required(),
            [
                Component {
                    id: "alpha".into(),
                    purpose: Purpose::TextInput,
                },
                Component {
                    id: "indicator".into(),
                    purpose: Purpose::StatusSurface,
                },
            ]
        );
        assert_eq!(
            gtk4::glib::MainContext::new().block_on(components.status("alpha")),
            ComponentStatus::Active
        );
        assert!(matches!(
            gtk4::glib::MainContext::new().block_on(components.enable("alpha")),
            Err(ComponentError::Failed {
                kind: StepKind::Setting,
                ..
            })
        ));
        assert!(desktop.enabled.borrow().is_empty());
    }

    #[test]
    fn an_unavailable_or_broken_provider_never_falls_back_to_ibus() {
        for (records, sockets, expected) in [
            (
                vec![ManifestRecord {
                    file_id: "alpha".into(),
                    manifest: Ok(manifest("alpha")),
                }],
                Vec::new(),
                ComponentStatus::Unavailable,
            ),
            (
                vec![ManifestRecord {
                    file_id: "alpha".into(),
                    manifest: Err("bad json".into()),
                }],
                vec!["alpha"],
                ComponentStatus::Blocked(Blocker::Incompatible),
            ),
        ] {
            let (components, desktop) = provider_components(records, &sockets, "alpha");
            assert_eq!(
                components.required()[0],
                Component {
                    id: "alpha".into(),
                    purpose: Purpose::TextInput,
                }
            );
            assert_eq!(
                gtk4::glib::MainContext::new().block_on(components.status("alpha")),
                expected
            );
            assert_eq!(
                gtk4::glib::MainContext::new().block_on(components.status("ibus")),
                ComponentStatus::Active
            );
            assert!(desktop.enabled.borrow().is_empty());
        }
    }

    #[test]
    fn no_active_provider_delegates_every_component_unchanged() {
        let (components, desktop) = provider_components(
            vec![ManifestRecord {
                file_id: "alpha".into(),
                manifest: Ok(manifest("alpha")),
            }],
            &["alpha"],
            "",
        );
        assert_eq!(
            components.required(),
            [
                Component {
                    id: "ibus".into(),
                    purpose: Purpose::TextInput,
                },
                Component {
                    id: "indicator".into(),
                    purpose: Purpose::StatusSurface,
                },
            ]
        );
        assert_eq!(
            gtk4::glib::MainContext::new().block_on(components.status("ibus")),
            ComponentStatus::Active
        );
        gtk4::glib::MainContext::new()
            .block_on(components.enable("ibus"))
            .unwrap();
        assert_eq!(*desktop.enabled.borrow(), ["ibus"]);
    }

    struct FakeSettings {
        rows: Vec<crate::domain::ClientSettingMetadata>,
        value: RefCell<String>,
        user_set: Cell<bool>,
        writes: RefCell<Vec<String>>,
        write_error: RefCell<Option<crate::ports::ClientSettingsError>>,
    }

    struct NoSubscription;
    impl crate::ports::ClientSettingsSubscription for NoSubscription {}

    impl crate::ports::ClientSettings for FakeSettings {
        fn list(
            &self,
        ) -> Result<Vec<crate::domain::ClientSettingMetadata>, crate::ports::ClientSettingsError>
        {
            Ok(self.rows.clone())
        }

        fn get(
            &self,
            _key: &str,
        ) -> Result<crate::domain::ClientSettingValue, crate::ports::ClientSettingsError> {
            Ok(crate::domain::ClientSettingValue::Text(
                self.value.borrow().clone(),
            ))
        }

        fn set(
            &self,
            _key: &str,
            value: crate::domain::ClientSettingValue,
        ) -> Result<(), crate::ports::ClientSettingsError> {
            if let Some(error) = self.write_error.borrow_mut().take() {
                return Err(error);
            }
            let text = value
                .as_str()
                .expect("the extension setting is text")
                .to_owned();
            self.writes.borrow_mut().push(text.clone());
            *self.value.borrow_mut() = text;
            self.user_set.set(true);
            Ok(())
        }

        fn has_user_value(&self, _key: &str) -> Result<bool, crate::ports::ClientSettingsError> {
            Ok(self.user_set.get())
        }

        fn subscribe(
            &self,
            _callback: crate::ports::ClientSettingsCallback,
        ) -> Result<
            Box<dyn crate::ports::ClientSettingsSubscription>,
            crate::ports::ClientSettingsError,
        > {
            Ok(Box::new(NoSubscription))
        }
    }

    fn settings_controller(active: &str) -> (Rc<FakeSettings>, Rc<MynaSettingsController>) {
        use crate::domain::{
            ClientSettingKey, ClientSettingMetadata, ClientSettingValue, SettingRange,
        };

        let key = ClientSettingKey::new(KEY_INPUT_METHOD_EXTENSION).unwrap();
        let row = ClientSettingMetadata::new(
            key,
            None,
            None,
            ClientSettingValue::Text(String::new()),
            SettingRange::Unrestricted,
            ClientSettingValue::Text(active.to_owned()),
            true,
        );
        let fake = Rc::new(FakeSettings {
            rows: vec![row],
            value: RefCell::new(active.to_owned()),
            user_set: Cell::new(!active.is_empty()),
            writes: RefCell::new(Vec::new()),
            write_error: RefCell::new(None),
        });
        let controller =
            MynaSettingsController::load(Rc::clone(&fake) as Rc<dyn crate::ports::ClientSettings>);
        (fake, controller)
    }

    struct RestartConfigurator {
        result: RefCell<Option<Result<(), crate::ports::SystemConfiguratorError>>>,
        calls: Cell<u32>,
    }

    impl RestartConfigurator {
        fn new(result: Result<(), crate::ports::SystemConfiguratorError>) -> Rc<Self> {
            Rc::new(Self {
                result: RefCell::new(Some(result)),
                calls: Cell::new(0),
            })
        }
    }

    #[async_trait::async_trait(?Send)]
    impl SystemConfigurator for RestartConfigurator {
        async fn execute_backend_switch(
            &self,
            _plan: &crate::active_backend::SwitchPlan,
            _cancellation: CancellationToken,
        ) -> Result<Vec<crate::domain::CommandResult>, crate::ports::SystemConfiguratorFailure>
        {
            unreachable!("extension lifecycle only restarts")
        }

        async fn restart_myna(
            &self,
            _cancellation: CancellationToken,
        ) -> Result<(), crate::ports::SystemConfiguratorError> {
            self.calls.set(self.calls.get() + 1);
            self.result
                .borrow_mut()
                .take()
                .expect("one restart per extension change")
        }

        async fn apply_backend_config(
            &self,
            _preview: &crate::backend_apply::ApplyPreview,
            _cancellation: CancellationToken,
        ) -> Result<Vec<crate::domain::CommandResult>, crate::ports::SystemConfiguratorFailure>
        {
            unreachable!("extension lifecycle only restarts")
        }

        async fn user_daemons_enabled(
            &self,
            _cancellation: CancellationToken,
        ) -> Result<bool, String> {
            unreachable!("extension lifecycle only restarts")
        }

        async fn set_up(
            &self,
            _plan: &crate::ports::SetUpPlan,
            _cancellation: CancellationToken,
        ) -> Result<(), crate::ports::SystemConfiguratorError> {
            unreachable!("extension lifecycle only restarts")
        }

        async fn install_snap(
            &self,
            _snap: &str,
            _cancellation: CancellationToken,
        ) -> Result<Option<String>, crate::ports::SystemConfiguratorError> {
            unreachable!("extension lifecycle only restarts")
        }

        async fn snap_change(
            &self,
            _change_id: &str,
            _cancellation: CancellationToken,
        ) -> Result<crate::snap_changes::ChangeInProgress, String> {
            unreachable!("extension lifecycle only restarts")
        }
    }

    fn persist(
        settings: &FakeSettings,
        controller: &MynaSettingsController,
        change: &ExtensionChange,
    ) -> bool {
        let request = change.request();
        let result = request.clone().persist(settings);
        let written = matches!(
            &result,
            Ok(crate::myna_settings::PersistenceCompletion::Written(_))
        );
        controller.complete(request, result);
        written
    }

    fn lifecycle(
        registry: Rc<dyn ExtensionRegistry>,
        active: &str,
    ) -> (
        Rc<ExtensionLifecycle>,
        Rc<FakeSettings>,
        OperationCoordinator,
    ) {
        let (settings, controller) = settings_controller(active);
        let operations = OperationCoordinator::new();
        let lifecycle = ExtensionLifecycle::new(registry, controller, operations.clone());
        (lifecycle, settings, operations)
    }

    #[test]
    fn enabling_writes_only_the_validated_id_and_restarts_myna() {
        let registry = registry(
            vec![ManifestRecord {
                file_id: "alpha".into(),
                manifest: Ok(manifest("alpha")),
            }],
            &["alpha"],
        );
        let (lifecycle, settings, operations) = lifecycle(registry, "");
        let change = lifecycle.begin_user_change(Some("alpha")).unwrap().unwrap();

        assert!(lifecycle.busy());
        assert!(persist(&settings, &lifecycle.controller(), &change));
        let ExtensionSaveOutcome::Restart(change) = lifecycle.save_finished(change, true) else {
            panic!("a saved selection must restart Myna");
        };
        let configurator = RestartConfigurator::new(Ok(()));
        assert_eq!(
            gtk4::glib::MainContext::new()
                .block_on(lifecycle.restart(change, configurator.as_ref())),
            ExtensionRestartOutcome::Succeeded
        );

        assert_eq!(settings.writes.borrow().as_slice(), ["alpha"]);
        assert_eq!(configurator.calls.get(), 1);
        assert!(!lifecycle.busy());
        assert_eq!(operations.active(), None);
        assert_eq!(
            lifecycle.snapshot().unwrap().active().unwrap().status,
            ExtensionStatus::Enabled
        );
    }

    #[test]
    fn disabling_clears_the_selection_without_removing_the_extension() {
        let registry = registry(
            vec![ManifestRecord {
                file_id: "alpha".into(),
                manifest: Ok(manifest("alpha")),
            }],
            &["alpha"],
        );
        let (lifecycle, settings, _) = lifecycle(registry, "alpha");
        let change = lifecycle.begin_user_change(None).unwrap().unwrap();

        assert!(persist(&settings, &lifecycle.controller(), &change));
        let ExtensionSaveOutcome::Restart(change) = lifecycle.save_finished(change, true) else {
            panic!("a disabled selection must restart Myna");
        };
        let configurator = RestartConfigurator::new(Ok(()));
        assert_eq!(
            gtk4::glib::MainContext::new()
                .block_on(lifecycle.restart(change, configurator.as_ref())),
            ExtensionRestartOutcome::Succeeded
        );

        assert_eq!(settings.writes.borrow().as_slice(), [""]);
        let snapshot = lifecycle.snapshot().unwrap();
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.entries[0].id, "alpha");
        assert_eq!(snapshot.entries[0].status, ExtensionStatus::Disabled);
    }

    #[test]
    fn a_missing_manifest_clears_stale_state_with_an_explanation() {
        let (lifecycle, settings, _) = lifecycle(registry(Vec::new(), &[]), "removed");
        assert_eq!(
            lifecycle.snapshot().unwrap().stale_active.as_deref(),
            Some("removed")
        );
        let change = lifecycle.begin_stale_cleanup().unwrap().unwrap();
        assert_eq!(change.cause(), ExtensionChangeCause::StaleSelection);

        assert!(persist(&settings, &lifecycle.controller(), &change));
        let ExtensionSaveOutcome::Restart(change) = lifecycle.save_finished(change, true) else {
            panic!("clearing stale state must restart Myna");
        };
        let configurator = RestartConfigurator::new(Ok(()));
        assert_eq!(
            gtk4::glib::MainContext::new()
                .block_on(lifecycle.restart(change, configurator.as_ref())),
            ExtensionRestartOutcome::Succeeded
        );

        assert_eq!(settings.writes.borrow().as_slice(), [""]);
        assert!(active_id(&lifecycle.controller()).is_none());
    }

    #[test]
    fn a_failed_save_keeps_the_previous_selection_and_does_not_restart() {
        let registry = registry(
            vec![ManifestRecord {
                file_id: "alpha".into(),
                manifest: Ok(manifest("alpha")),
            }],
            &["alpha"],
        );
        let (lifecycle, settings, operations) = lifecycle(registry, "alpha");
        *settings.write_error.borrow_mut() =
            Some(crate::ports::ClientSettingsError::StoreUnavailable {
                message: "disk is read-only".into(),
            });
        let change = lifecycle.begin_user_change(None).unwrap().unwrap();

        assert!(!persist(&settings, &lifecycle.controller(), &change));
        let outcome = lifecycle.save_finished(change, false);
        assert!(matches!(
            outcome,
            ExtensionSaveOutcome::Failed(ExtensionChangeCause::User)
        ));
        assert_eq!(active_id(&lifecycle.controller()).as_deref(), Some("alpha"));
        assert!(!lifecycle.busy());
        assert_eq!(operations.active(), None);
    }

    #[test]
    fn a_failed_restart_is_reported_and_releases_the_operation_gate() {
        let registry = registry(
            vec![ManifestRecord {
                file_id: "alpha".into(),
                manifest: Ok(manifest("alpha")),
            }],
            &["alpha"],
        );
        let (lifecycle, settings, operations) = lifecycle(registry, "");
        let change = lifecycle.begin_user_change(Some("alpha")).unwrap().unwrap();
        assert!(persist(&settings, &lifecycle.controller(), &change));
        let ExtensionSaveOutcome::Restart(change) = lifecycle.save_finished(change, true) else {
            panic!("a saved selection must attempt a restart");
        };
        let configurator =
            RestartConfigurator::new(Err(crate::ports::SystemConfiguratorError::Cancelled));

        assert!(matches!(
            gtk4::glib::MainContext::new()
                .block_on(lifecycle.restart(change, configurator.as_ref())),
            ExtensionRestartOutcome::Failed(_)
        ));
        assert!(!lifecycle.busy());
        assert_eq!(operations.active(), None);
    }

    #[test]
    fn another_settings_operation_refuses_a_toggle_and_stale_cleanup_retries_later() {
        let (lifecycle, _, operations) = lifecycle(
            registry(
                vec![ManifestRecord {
                    file_id: "alpha".into(),
                    manifest: Ok(manifest("alpha")),
                }],
                &["alpha"],
            ),
            "removed",
        );
        let backend = operations.begin(OperationKind::BackendSwitch).unwrap();

        assert_eq!(
            lifecycle.begin_user_change(Some("alpha")).unwrap_err(),
            ExtensionActionError::Busy
        );
        assert!(lifecycle.begin_stale_cleanup().unwrap().is_none());
        assert!(!lifecycle.busy());

        operations.complete(backend.token());
        assert!(lifecycle.begin_stale_cleanup().unwrap().is_some());
    }
}
