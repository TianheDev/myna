//! Install and remove AppStream-advertised IM extension packages.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::command::CancellationToken;
use crate::domain::ClientSettingValue;
use crate::extensions::{active_id, ExtensionManager, ExtensionRegistry};
use crate::myna_settings::{MynaSettingsController, PersistenceCompletion, PersistenceWriter};
use crate::operation_gate::{BeginOperationError, OperationCoordinator, OperationKind};
use crate::ports::{SystemConfigurator, SystemConfiguratorError};
use myna_core::settings::KEY_INPUT_METHOD_EXTENSION;

pub const MYNA_COMPONENT_ID: &str = "com.canonical.Myna.Config";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionPackage {
    pub component_id: String,
    pub package_name: String,
    pub name: String,
    pub summary: String,
    pub installed: bool,
    /// Provider ids declared by the package's installed manifest paths.
    pub provider_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageProgress {
    Status(String),
    Percent(u8),
}

pub type PackageProgressCallback = Arc<dyn Fn(PackageProgress) + Send + Sync + 'static>;

pub fn no_progress() -> PackageProgressCallback {
    Arc::new(|_| {})
}

#[async_trait(?Send)]
pub trait ExtensionPackageManager {
    async fn discover(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<ExtensionPackage>, PackageManagerError>;

    async fn install(
        &self,
        package: &ExtensionPackage,
        cancellation: CancellationToken,
        progress: PackageProgressCallback,
    ) -> Result<(), PackageManagerError>;

    async fn remove(
        &self,
        package: &ExtensionPackage,
        cancellation: CancellationToken,
        progress: PackageProgressCallback,
    ) -> Result<(), PackageManagerError>;
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum PackageManagerError {
    #[error("PackageKit is unavailable: {0}")]
    Unavailable(String),
    #[error("package {0} was not found in the configured software sources")]
    NotFound(String),
    #[error("installing or removing {package} was not authorized: {detail}")]
    Denied { package: String, detail: String },
    #[error("PackageKit could not change {package}: {detail}")]
    Failed { package: String, detail: String },
    #[error("the package operation was cancelled")]
    Cancelled,
}

impl PackageManagerError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum PackageOperationError {
    #[error("another operation is already active")]
    Busy,
    #[error("the input-method selection could not be saved: {0}")]
    Save(String),
    #[error("Myna could not restart: {0}")]
    Restart(String),
    #[error(transparent)]
    Package(#[from] PackageManagerError),
    #[error(
        "the package operation failed ({operation}), and restoring the previous input-method selection also failed: {restore}"
    )]
    RestoreFailed { operation: String, restore: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageOperationOutcome {
    Installed,
    Removed,
    RemovedWithoutActiveProvider,
}

pub struct ExtensionPackageCoordinator {
    packages: Rc<dyn ExtensionPackageManager>,
    registry: Rc<dyn ExtensionRegistry>,
    settings: Rc<MynaSettingsController>,
    writer: PersistenceWriter,
    configurator: Rc<dyn SystemConfigurator>,
    operations: OperationCoordinator,
    busy: Cell<bool>,
}

impl ExtensionPackageCoordinator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        packages: Rc<dyn ExtensionPackageManager>,
        registry: Rc<dyn ExtensionRegistry>,
        settings: Rc<MynaSettingsController>,
        writer: PersistenceWriter,
        configurator: Rc<dyn SystemConfigurator>,
        operations: OperationCoordinator,
    ) -> Rc<Self> {
        Rc::new(Self {
            packages,
            registry,
            settings,
            writer,
            configurator,
            operations,
            busy: Cell::new(false),
        })
    }

    pub fn busy(&self) -> bool {
        self.busy.get()
    }

    pub async fn discover(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<ExtensionPackage>, PackageManagerError> {
        self.packages.discover(cancellation).await
    }

    pub async fn install(
        &self,
        package: &ExtensionPackage,
        cancellation: CancellationToken,
        progress: PackageProgressCallback,
    ) -> Result<PackageOperationOutcome, PackageOperationError> {
        let token = self.begin()?;
        let result = self.packages.install(package, cancellation, progress).await;
        self.finish(token);
        result
            .map(|()| PackageOperationOutcome::Installed)
            .map_err(PackageOperationError::Package)
    }

    pub async fn remove(
        &self,
        package: &ExtensionPackage,
        cancellation: CancellationToken,
        progress: PackageProgressCallback,
    ) -> Result<PackageOperationOutcome, PackageOperationError> {
        let token = self.begin()?;
        let previous = active_id(&self.settings);
        let disables_active = previous
            .as_ref()
            .is_some_and(|id| package.provider_ids.iter().any(|candidate| candidate == id));

        if disables_active {
            if let Err(error) = self.write_selection(None).await {
                self.finish(token);
                return Err(error);
            }
            if let Err(error) = self.restart().await {
                return Err(self
                    .restore_after_failure(
                        token,
                        previous.as_deref(),
                        PackageOperationError::Restart(error),
                    )
                    .await);
            }
        }

        let removal = self.packages.remove(package, cancellation, progress).await;
        match removal {
            Ok(()) => {
                self.finish(token);
                Ok(if disables_active {
                    PackageOperationOutcome::Removed
                } else {
                    PackageOperationOutcome::RemovedWithoutActiveProvider
                })
            }
            Err(error) if disables_active => Err(self
                .restore_after_failure(
                    token,
                    previous.as_deref(),
                    PackageOperationError::Package(error),
                )
                .await),
            Err(error) => {
                self.finish(token);
                Err(PackageOperationError::Package(error))
            }
        }
    }

    fn begin(&self) -> Result<u64, PackageOperationError> {
        if self.busy.replace(true) {
            return Err(PackageOperationError::Busy);
        }
        match self.operations.begin(OperationKind::ExtensionPackage) {
            Ok(operation) => Ok(operation.token()),
            Err(BeginOperationError::Busy) => {
                self.busy.set(false);
                Err(PackageOperationError::Busy)
            }
        }
    }

    fn finish(&self, token: u64) {
        self.operations.complete(token);
        self.busy.set(false);
    }

    async fn write_selection(&self, selection: Option<&str>) -> Result<(), PackageOperationError> {
        let request = self
            .settings
            .set(
                KEY_INPUT_METHOD_EXTENSION,
                ClientSettingValue::Text(selection.unwrap_or_default().to_owned()),
            )
            .map_err(|error| PackageOperationError::Save(error.to_string()))?;
        let job = self
            .writer
            .submit(request.clone())
            .map_err(|error| PackageOperationError::Save(error.to_string()))?;
        let result = gio::spawn_blocking(move || job.wait())
            .await
            .unwrap_or_else(|_| {
                Err(crate::ports::ClientSettingsError::StoreUnavailable {
                    message: "the settings persistence worker terminated unexpectedly".into(),
                })
            });
        let written = matches!(&result, Ok(PersistenceCompletion::Written(_)));
        self.settings.complete(request, result);
        if written {
            Ok(())
        } else {
            Err(PackageOperationError::Save(
                "the settings store did not confirm the new value".into(),
            ))
        }
    }

    async fn restart(&self) -> Result<(), String> {
        self.configurator
            .restart_myna(CancellationToken::new())
            .await
            .map_err(|error| match error {
                SystemConfiguratorError::Cancelled => "the restart was cancelled".into(),
                error => error.to_string(),
            })
    }

    async fn restore_after_failure(
        &self,
        token: u64,
        previous: Option<&str>,
        operation: PackageOperationError,
    ) -> PackageOperationError {
        let provider_still_available = previous.is_some_and(|id| {
            ExtensionManager::new(Rc::clone(&self.registry))
                .selection_allowed(None, id)
                .unwrap_or(false)
        });
        if provider_still_available {
            if let Err(restore) = self.write_selection(previous).await {
                self.finish(token);
                return PackageOperationError::RestoreFailed {
                    operation: operation.to_string(),
                    restore: restore.to_string(),
                };
            }
            if let Err(restore) = self.restart().await {
                self.finish(token);
                return PackageOperationError::RestoreFailed {
                    operation: operation.to_string(),
                    restore,
                };
            }
        }
        self.finish(token);
        operation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    };

    use crate::domain::{ClientSettingKey, ClientSettingMetadata, CommandResult, SettingRange};
    use crate::extensions::{ManifestRecord, ProviderManifest};
    use crate::myna_settings::MynaSettingsController;
    use crate::operation_gate::OperationKind;
    use crate::ports::{
        ClientSettings, ClientSettingsCallback, ClientSettingsError, ClientSettingsSubscription,
        SystemConfigurator, SystemConfiguratorError, SystemConfiguratorFailure,
    };

    struct SharedSettings {
        value: Mutex<String>,
        user_value: AtomicBool,
        writes: Mutex<Vec<String>>,
        write_error: Mutex<Option<ClientSettingsError>>,
    }

    #[derive(Clone)]
    struct SettingsHandle(Arc<SharedSettings>);

    struct NoSubscription;
    impl ClientSettingsSubscription for NoSubscription {}

    impl ClientSettings for SettingsHandle {
        fn list(&self) -> Result<Vec<ClientSettingMetadata>, ClientSettingsError> {
            Ok(vec![ClientSettingMetadata::new(
                ClientSettingKey::new(KEY_INPUT_METHOD_EXTENSION).unwrap(),
                None,
                None,
                ClientSettingValue::Text(String::new()),
                SettingRange::Unrestricted,
                ClientSettingValue::Text(self.0.value.lock().unwrap().clone()),
                true,
            )])
        }

        fn get(&self, _key: &str) -> Result<ClientSettingValue, ClientSettingsError> {
            Ok(ClientSettingValue::Text(
                self.0.value.lock().unwrap().clone(),
            ))
        }

        fn set(&self, _key: &str, value: ClientSettingValue) -> Result<(), ClientSettingsError> {
            if let Some(error) = self.0.write_error.lock().unwrap().take() {
                return Err(error);
            }
            let value = value
                .as_str()
                .expect("the input-method setting is text")
                .to_owned();
            self.0.writes.lock().unwrap().push(value.clone());
            *self.0.value.lock().unwrap() = value;
            self.0.user_value.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn has_user_value(&self, _key: &str) -> Result<bool, ClientSettingsError> {
            Ok(self.0.user_value.load(Ordering::SeqCst))
        }

        fn subscribe(
            &self,
            _callback: ClientSettingsCallback,
        ) -> Result<Box<dyn ClientSettingsSubscription>, ClientSettingsError> {
            Ok(Box::new(NoSubscription))
        }
    }

    struct FakeRegistry {
        provider_available: bool,
    }

    impl ExtensionRegistry for FakeRegistry {
        fn records(&self) -> Result<Vec<ManifestRecord>, String> {
            if !self.provider_available {
                return Ok(Vec::new());
            }
            Ok(vec![ManifestRecord {
                file_id: "alpha".into(),
                manifest: Ok(ProviderManifest {
                    schema_version: 1,
                    id: "alpha".into(),
                    name: "Alpha".into(),
                    protocols: vec![1],
                    socket: "alpha.sock".into(),
                    capabilities: crate::extensions::ManifestCapabilities {
                        preedit: true,
                        surrounding_text: true,
                        secure_field_detection: true,
                    },
                }),
            }])
        }

        fn socket_present(&self, _id: &str) -> bool {
            true
        }
    }

    struct FakePackageManager {
        install_result: RefCell<Option<Result<(), PackageManagerError>>>,
        remove_result: RefCell<Option<Result<(), PackageManagerError>>>,
        installs: Cell<u32>,
        removals: Cell<u32>,
        progress: RefCell<Vec<PackageProgress>>,
    }

    impl FakePackageManager {
        fn new(
            install_result: Result<(), PackageManagerError>,
            remove_result: Result<(), PackageManagerError>,
        ) -> Rc<Self> {
            Rc::new(Self {
                install_result: RefCell::new(Some(install_result)),
                remove_result: RefCell::new(Some(remove_result)),
                installs: Cell::new(0),
                removals: Cell::new(0),
                progress: RefCell::new(Vec::new()),
            })
        }
    }

    #[async_trait(?Send)]
    impl ExtensionPackageManager for FakePackageManager {
        async fn discover(
            &self,
            _cancellation: CancellationToken,
        ) -> Result<Vec<ExtensionPackage>, PackageManagerError> {
            Ok(Vec::new())
        }

        async fn install(
            &self,
            _package: &ExtensionPackage,
            _cancellation: CancellationToken,
            progress: PackageProgressCallback,
        ) -> Result<(), PackageManagerError> {
            self.installs.set(self.installs.get() + 1);
            progress(PackageProgress::Percent(42));
            self.progress
                .borrow_mut()
                .push(PackageProgress::Percent(42));
            self.install_result
                .borrow_mut()
                .take()
                .expect("one install")
        }

        async fn remove(
            &self,
            _package: &ExtensionPackage,
            _cancellation: CancellationToken,
            progress: PackageProgressCallback,
        ) -> Result<(), PackageManagerError> {
            self.removals.set(self.removals.get() + 1);
            progress(PackageProgress::Status("Removing".into()));
            self.remove_result.borrow_mut().take().expect("one removal")
        }
    }

    struct RestartConfigurator {
        results: RefCell<VecDeque<Result<(), SystemConfiguratorError>>>,
        calls: Cell<u32>,
    }

    impl RestartConfigurator {
        fn successful() -> Rc<Self> {
            Self::with_results([Ok(()), Ok(())])
        }

        fn with_results(
            results: impl IntoIterator<Item = Result<(), SystemConfiguratorError>>,
        ) -> Rc<Self> {
            Rc::new(Self {
                results: RefCell::new(results.into_iter().collect()),
                calls: Cell::new(0),
            })
        }
    }

    #[async_trait(?Send)]
    impl SystemConfigurator for RestartConfigurator {
        async fn execute_backend_switch(
            &self,
            _plan: &crate::active_backend::SwitchPlan,
            _cancellation: CancellationToken,
        ) -> Result<Vec<CommandResult>, SystemConfiguratorFailure> {
            unreachable!("package operations only restart")
        }

        async fn restart_myna(
            &self,
            _cancellation: CancellationToken,
        ) -> Result<(), SystemConfiguratorError> {
            self.calls.set(self.calls.get() + 1);
            self.results.borrow_mut().pop_front().unwrap_or(Ok(()))
        }

        async fn apply_backend_config(
            &self,
            _preview: &crate::backend_apply::ApplyPreview,
            _cancellation: CancellationToken,
        ) -> Result<Vec<CommandResult>, SystemConfiguratorFailure> {
            unreachable!("package operations only restart")
        }

        async fn user_daemons_enabled(
            &self,
            _cancellation: CancellationToken,
        ) -> Result<bool, String> {
            unreachable!("package operations only restart")
        }

        async fn set_up(
            &self,
            _plan: &crate::ports::SetUpPlan,
            _cancellation: CancellationToken,
        ) -> Result<(), SystemConfiguratorError> {
            unreachable!("package operations only restart")
        }

        async fn install_snap(
            &self,
            _snap: &str,
            _cancellation: CancellationToken,
        ) -> Result<Option<String>, SystemConfiguratorError> {
            unreachable!("package operations only restart")
        }

        async fn snap_change(
            &self,
            _change_id: &str,
            _cancellation: CancellationToken,
        ) -> Result<crate::snap_changes::ChangeInProgress, String> {
            unreachable!("package operations only restart")
        }
    }

    struct Harness {
        coordinator: Rc<ExtensionPackageCoordinator>,
        settings: Arc<SharedSettings>,
        packages: Rc<FakePackageManager>,
        configurator: Rc<RestartConfigurator>,
        operations: OperationCoordinator,
    }

    fn harness(
        active: &str,
        install_result: Result<(), PackageManagerError>,
        remove_result: Result<(), PackageManagerError>,
    ) -> Harness {
        let settings = Arc::new(SharedSettings {
            value: Mutex::new(active.into()),
            user_value: AtomicBool::new(!active.is_empty()),
            writes: Mutex::new(Vec::new()),
            write_error: Mutex::new(None),
        });
        let handle = SettingsHandle(Arc::clone(&settings));
        let controller =
            MynaSettingsController::load(Rc::new(handle.clone()) as Rc<dyn ClientSettings>);
        let writer = PersistenceWriter::spawn(move || Ok(handle));
        let packages = FakePackageManager::new(install_result, remove_result);
        let configurator = RestartConfigurator::successful();
        let operations = OperationCoordinator::new();
        let coordinator = ExtensionPackageCoordinator::new(
            packages.clone(),
            Rc::new(FakeRegistry {
                provider_available: true,
            }),
            controller,
            writer,
            configurator.clone(),
            operations.clone(),
        );
        Harness {
            coordinator,
            settings,
            packages,
            configurator,
            operations,
        }
    }

    fn package() -> ExtensionPackage {
        ExtensionPackage {
            component_id: "org.example.Myna.Alpha".into(),
            package_name: "myna-alpha".into(),
            name: "Alpha support".into(),
            summary: "Adds Alpha".into(),
            installed: true,
            provider_ids: vec!["alpha".into()],
        }
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        gtk4::glib::MainContext::new().block_on(future)
    }

    #[test]
    fn install_uses_the_package_manager_without_activating_the_extension() {
        let harness = harness("", Ok(()), Ok(()));
        let outcome = block_on(harness.coordinator.install(
            &package(),
            CancellationToken::new(),
            no_progress(),
        ))
        .unwrap();

        assert_eq!(outcome, PackageOperationOutcome::Installed);
        assert_eq!(harness.packages.installs.get(), 1);
        assert_eq!(
            harness.packages.progress.borrow().as_slice(),
            [PackageProgress::Percent(42)]
        );
        assert!(harness.settings.writes.lock().unwrap().is_empty());
        assert_eq!(harness.configurator.calls.get(), 0);
        assert!(!harness.coordinator.busy());
        assert_eq!(harness.operations.active(), None);
    }

    #[test]
    fn install_failure_is_reported_without_touching_settings() {
        let harness = harness(
            "",
            Err(PackageManagerError::Failed {
                package: "Alpha support".into(),
                detail: "not available".into(),
            }),
            Ok(()),
        );
        let error = block_on(harness.coordinator.install(
            &package(),
            CancellationToken::new(),
            no_progress(),
        ))
        .unwrap_err();

        assert!(matches!(error, PackageOperationError::Package(_)));
        assert!(harness.settings.writes.lock().unwrap().is_empty());
        assert_eq!(harness.configurator.calls.get(), 0);
        assert_eq!(harness.operations.active(), None);
    }

    #[test]
    fn removing_the_active_extension_disables_it_before_removal() {
        let harness = harness("alpha", Ok(()), Ok(()));
        let outcome = block_on(harness.coordinator.remove(
            &package(),
            CancellationToken::new(),
            no_progress(),
        ))
        .unwrap();

        assert_eq!(outcome, PackageOperationOutcome::Removed);
        assert_eq!(harness.settings.writes.lock().unwrap().as_slice(), [""]);
        assert_eq!(harness.configurator.calls.get(), 1);
        assert_eq!(harness.packages.removals.get(), 1);
        assert_eq!(harness.settings.value.lock().unwrap().as_str(), "");
    }

    #[test]
    fn failed_removal_restores_the_previous_selection() {
        let harness = harness(
            "alpha",
            Ok(()),
            Err(PackageManagerError::Failed {
                package: "Alpha support".into(),
                detail: "disk full".into(),
            }),
        );
        let error = block_on(harness.coordinator.remove(
            &package(),
            CancellationToken::new(),
            no_progress(),
        ))
        .unwrap_err();

        assert!(matches!(error, PackageOperationError::Package(_)));
        assert_eq!(
            harness.settings.writes.lock().unwrap().as_slice(),
            ["", "alpha"]
        );
        assert_eq!(harness.settings.value.lock().unwrap().as_str(), "alpha");
        assert_eq!(harness.configurator.calls.get(), 2);
        assert_eq!(harness.operations.active(), None);
    }

    #[test]
    fn cancelled_removal_also_restores_when_the_provider_is_available() {
        let harness = harness("alpha", Ok(()), Err(PackageManagerError::Cancelled));
        let error = block_on(harness.coordinator.remove(
            &package(),
            CancellationToken::new(),
            no_progress(),
        ))
        .unwrap_err();

        assert_eq!(
            error,
            PackageOperationError::Package(PackageManagerError::Cancelled)
        );
        assert_eq!(harness.settings.value.lock().unwrap().as_str(), "alpha");
        assert_eq!(harness.configurator.calls.get(), 2);
    }

    #[test]
    fn a_restart_failure_after_disabling_restores_before_package_removal() {
        let mut harness = harness("alpha", Ok(()), Ok(()));
        harness.configurator =
            RestartConfigurator::with_results([Err(SystemConfiguratorError::Cancelled), Ok(())]);
        // Rebuild only the coordinator so it uses the replacement configurator.
        let handle = SettingsHandle(Arc::clone(&harness.settings));
        let controller =
            MynaSettingsController::load(Rc::new(handle.clone()) as Rc<dyn ClientSettings>);
        harness.coordinator = ExtensionPackageCoordinator::new(
            harness.packages.clone(),
            Rc::new(FakeRegistry {
                provider_available: true,
            }),
            controller,
            PersistenceWriter::spawn(move || Ok(handle)),
            harness.configurator.clone(),
            harness.operations.clone(),
        );

        let error = block_on(harness.coordinator.remove(
            &package(),
            CancellationToken::new(),
            no_progress(),
        ))
        .unwrap_err();

        assert!(
            matches!(error, PackageOperationError::Restart(_)),
            "{error:?}"
        );
        assert_eq!(
            harness.settings.writes.lock().unwrap().as_slice(),
            ["", "alpha"]
        );
        assert_eq!(harness.packages.removals.get(), 0);
        assert_eq!(harness.configurator.calls.get(), 2);
    }

    #[test]
    fn a_failed_removal_does_not_restore_a_provider_that_has_disappeared() {
        let mut harness = harness(
            "alpha",
            Ok(()),
            Err(PackageManagerError::Failed {
                package: "Alpha support".into(),
                detail: "package disappeared".into(),
            }),
        );
        let handle = SettingsHandle(Arc::clone(&harness.settings));
        let controller =
            MynaSettingsController::load(Rc::new(handle.clone()) as Rc<dyn ClientSettings>);
        harness.coordinator = ExtensionPackageCoordinator::new(
            harness.packages.clone(),
            Rc::new(FakeRegistry {
                provider_available: false,
            }),
            controller,
            PersistenceWriter::spawn(move || Ok(handle)),
            harness.configurator.clone(),
            harness.operations.clone(),
        );

        let error = block_on(harness.coordinator.remove(
            &package(),
            CancellationToken::new(),
            no_progress(),
        ))
        .unwrap_err();

        assert!(matches!(error, PackageOperationError::Package(_)));
        assert_eq!(harness.settings.writes.lock().unwrap().as_slice(), [""]);
        assert_eq!(harness.settings.value.lock().unwrap().as_str(), "");
        assert_eq!(harness.configurator.calls.get(), 1);
    }

    #[test]
    fn a_concurrent_operation_refuses_the_package_change() {
        let harness = harness("", Ok(()), Ok(()));
        let held = harness
            .operations
            .begin(OperationKind::BackendSwitch)
            .unwrap();

        assert_eq!(
            block_on(harness.coordinator.install(
                &package(),
                CancellationToken::new(),
                no_progress(),
            ))
            .unwrap_err(),
            PackageOperationError::Busy
        );
        assert_eq!(harness.packages.installs.get(), 0);
        harness.operations.complete(held.token());
    }

    #[test]
    fn a_non_active_package_is_removed_without_restarting_myna() {
        let harness = harness("", Ok(()), Ok(()));
        let outcome = block_on(harness.coordinator.remove(
            &package(),
            CancellationToken::new(),
            no_progress(),
        ))
        .unwrap();

        assert_eq!(
            outcome,
            PackageOperationOutcome::RemovedWithoutActiveProvider
        );
        assert!(harness.settings.writes.lock().unwrap().is_empty());
        assert_eq!(harness.configurator.calls.get(), 0);
        assert_eq!(harness.packages.removals.get(), 1);
    }

    #[test]
    fn a_settings_failure_keeps_the_active_package_installed() {
        let harness = harness("alpha", Ok(()), Ok(()));
        *harness.settings.write_error.lock().unwrap() =
            Some(ClientSettingsError::StoreUnavailable {
                message: "store is read-only".into(),
            });

        let error = block_on(harness.coordinator.remove(
            &package(),
            CancellationToken::new(),
            no_progress(),
        ))
        .unwrap_err();

        assert!(matches!(error, PackageOperationError::Save(_)));
        assert_eq!(harness.packages.removals.get(), 0);
        assert_eq!(harness.configurator.calls.get(), 0);
        assert!(!harness.coordinator.busy());
    }

    #[test]
    fn package_manager_cancellation_is_exposed_as_cancelled() {
        assert!(PackageManagerError::Cancelled.is_cancelled());
        assert!(!PackageManagerError::NotFound("alpha".into()).is_cancelled());
    }
}
