use std::sync::{Arc, Mutex};

use gtk4::prelude::*;

use myna_config::command::CancellationToken;
use myna_config::domain::{
    ClientSettingKey, ClientSettingMetadata, ClientSettingValue, SettingRange,
};
use myna_config::extension_packages::{
    ExtensionPackage, ExtensionPackageManager, PackageManagerError, PackageProgressCallback,
};
use myna_config::extension_ui::{ExtensionUi, ExtensionUiPorts};
use myna_config::extensions::{
    ExtensionRegistry, ManifestCapabilities, ManifestRecord, ProviderManifest,
};
use myna_config::myna_settings::{MynaSettingsController, PersistenceWriter};
use myna_config::operation_gate::OperationCoordinator;
use myna_config::ports::{
    ClientSettings, ClientSettingsCallback, ClientSettingsError, ClientSettingsSubscription,
    SystemConfigurator, SystemConfiguratorError, SystemConfiguratorFailure,
};
use myna_config::ui::InstallControl;

struct TestSettings {
    value: Mutex<String>,
}

#[derive(Clone)]
struct SettingsHandle(Arc<TestSettings>);

struct NoSubscription;
impl ClientSettingsSubscription for NoSubscription {}

impl ClientSettings for SettingsHandle {
    fn list(&self) -> Result<Vec<ClientSettingMetadata>, ClientSettingsError> {
        Ok(vec![ClientSettingMetadata::new(
            ClientSettingKey::new(myna_core::settings::KEY_INPUT_METHOD_EXTENSION).unwrap(),
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
        *self.0.value.lock().unwrap() = value
            .as_str()
            .ok_or_else(|| ClientSettingsError::InvalidValue {
                key: "input-method-extension".into(),
                message: "expected text".into(),
            })?
            .to_owned();
        Ok(())
    }

    fn has_user_value(&self, _key: &str) -> Result<bool, ClientSettingsError> {
        Ok(!self.0.value.lock().unwrap().is_empty())
    }

    fn subscribe(
        &self,
        _callback: ClientSettingsCallback,
    ) -> Result<Box<dyn ClientSettingsSubscription>, ClientSettingsError> {
        Ok(Box::new(NoSubscription))
    }
}

struct TestRegistry {
    records: Vec<ManifestRecord>,
    fail: bool,
}

impl ExtensionRegistry for TestRegistry {
    fn records(&self) -> Result<Vec<ManifestRecord>, String> {
        if self.fail {
            return Err("registry unavailable".into());
        }
        Ok(self.records.clone())
    }

    fn socket_present(&self, _id: &str) -> bool {
        true
    }
}

struct TestPackages {
    offers: Vec<ExtensionPackage>,
    install_result: Result<(), PackageManagerError>,
    remove_result: Result<(), PackageManagerError>,
}

#[async_trait::async_trait(?Send)]
impl ExtensionPackageManager for TestPackages {
    async fn discover(
        &self,
        _cancellation: CancellationToken,
    ) -> Result<Vec<ExtensionPackage>, PackageManagerError> {
        Ok(self.offers.clone())
    }

    async fn install(
        &self,
        _package: &ExtensionPackage,
        _cancellation: CancellationToken,
        _progress: PackageProgressCallback,
    ) -> Result<(), PackageManagerError> {
        self.install_result.clone()
    }

    async fn remove(
        &self,
        _package: &ExtensionPackage,
        _cancellation: CancellationToken,
        _progress: PackageProgressCallback,
    ) -> Result<(), PackageManagerError> {
        self.remove_result.clone()
    }
}

struct TestConfigurator;

#[async_trait::async_trait(?Send)]
impl SystemConfigurator for TestConfigurator {
    async fn execute_backend_switch(
        &self,
        _plan: &myna_config::active_backend::SwitchPlan,
        _cancellation: CancellationToken,
    ) -> Result<Vec<myna_config::domain::CommandResult>, SystemConfiguratorFailure> {
        unreachable!()
    }

    async fn restart_myna(
        &self,
        _cancellation: CancellationToken,
    ) -> Result<(), SystemConfiguratorError> {
        Ok(())
    }

    async fn apply_backend_config(
        &self,
        _preview: &myna_config::backend_apply::ApplyPreview,
        _cancellation: CancellationToken,
    ) -> Result<Vec<myna_config::domain::CommandResult>, SystemConfiguratorFailure> {
        unreachable!()
    }

    async fn user_daemons_enabled(&self, _cancellation: CancellationToken) -> Result<bool, String> {
        unreachable!()
    }

    async fn set_up(
        &self,
        _plan: &myna_config::ports::SetUpPlan,
        _cancellation: CancellationToken,
    ) -> Result<(), SystemConfiguratorError> {
        unreachable!()
    }

    async fn install_snap(
        &self,
        _snap: &str,
        _cancellation: CancellationToken,
    ) -> Result<Option<String>, SystemConfiguratorError> {
        unreachable!()
    }

    async fn snap_change(
        &self,
        _change_id: &str,
        _cancellation: CancellationToken,
    ) -> Result<myna_config::snap_changes::ChangeInProgress, String> {
        unreachable!()
    }
}

fn manifest(id: &str) -> ProviderManifest {
    ProviderManifest {
        schema_version: 1,
        id: id.into(),
        name: format!("{id} provider"),
        protocols: vec![1],
        socket: format!("{id}.sock"),
        capabilities: ManifestCapabilities {
            preedit: true,
            surrounding_text: false,
            secure_field_detection: true,
        },
    }
}

fn package(name: &str, installed: bool, provider_ids: &[&str]) -> ExtensionPackage {
    ExtensionPackage {
        component_id: format!("org.example.{name}"),
        package_name: name.into(),
        name: name.into(),
        summary: format!("{name} summary"),
        installed,
        provider_ids: provider_ids.iter().map(|id| (*id).into()).collect(),
    }
}

fn pump() {
    for _ in 0..100 {
        while gtk4::glib::MainContext::default().iteration(false) {}
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

fn collect_controls(widget: &gtk4::Widget, controls: &mut Vec<InstallControl>) {
    let mut child = widget.first_child();
    while let Some(node) = child {
        if let Some(control) = node.downcast_ref::<InstallControl>() {
            controls.push(control.clone());
        }
        collect_controls(&node, controls);
        child = node.next_sibling();
    }
}

#[test]
fn extension_controls_render_and_toggle() {
    if std::env::var_os("MYNA_CONFIG_GTK_TESTS").is_none() {
        return;
    }
    let _ = gtk4::init();

    let settings = Arc::new(TestSettings {
        value: Mutex::new("alpha".into()),
    });
    let handle = SettingsHandle(Arc::clone(&settings));
    let controller = MynaSettingsController::load(
        std::rc::Rc::new(handle.clone()) as std::rc::Rc<dyn ClientSettings>
    );
    let group = libadwaita::PreferencesGroup::new();
    let _ui = ExtensionUi::new(
        group.clone(),
        libadwaita::ToastOverlay::new(),
        ExtensionUiPorts {
            registry: std::rc::Rc::new(TestRegistry {
                records: vec![ManifestRecord {
                    file_id: "alpha".into(),
                    manifest: Ok(manifest("alpha")),
                }],
                fail: false,
            }),
            package_manager: std::rc::Rc::new(TestPackages {
                offers: vec![
                    package("myna-alpha", true, &["alpha"]),
                    package("myna-beta", false, &["beta"]),
                ],
                install_result: Err(PackageManagerError::Failed {
                    package: "myna-beta".into(),
                    detail: "offline".into(),
                }),
                remove_result: Err(PackageManagerError::Failed {
                    package: "myna-alpha".into(),
                    detail: "offline".into(),
                }),
            }),
            controller,
            writer: PersistenceWriter::spawn(move || Ok(handle)),
            configurator: std::rc::Rc::new(TestConfigurator),
            operations: OperationCoordinator::new(),
        },
    );
    pump();

    assert!(group.first_child().is_some());

    let mut child = group.first_child();
    while let Some(widget) = child {
        if let Some(row) = widget.downcast_ref::<libadwaita::SwitchRow>() {
            row.set_active(false);
            break;
        }
        child = widget.next_sibling();
    }
    pump();
    assert!(group.first_child().is_some());

    let mut controls = Vec::new();
    collect_controls(group.upcast_ref(), &mut controls);
    assert_eq!(controls.len(), 2);
    for control in controls {
        control.button().emit_clicked();
    }
    pump();
    assert!(group.first_child().is_some());

    let failed_group = libadwaita::PreferencesGroup::new();
    let failed_settings = Arc::new(TestSettings {
        value: Mutex::new(String::new()),
    });
    let failed_handle = SettingsHandle(Arc::clone(&failed_settings));
    let failed_controller = MynaSettingsController::load(
        std::rc::Rc::new(failed_handle.clone()) as std::rc::Rc<dyn ClientSettings>
    );
    let _failed_ui = ExtensionUi::new(
        failed_group.clone(),
        libadwaita::ToastOverlay::new(),
        ExtensionUiPorts {
            registry: std::rc::Rc::new(TestRegistry {
                records: Vec::new(),
                fail: true,
            }),
            package_manager: std::rc::Rc::new(TestPackages {
                offers: Vec::new(),
                install_result: Ok(()),
                remove_result: Ok(()),
            }),
            controller: failed_controller,
            writer: PersistenceWriter::spawn(move || Ok(failed_handle)),
            configurator: std::rc::Rc::new(TestConfigurator),
            operations: OperationCoordinator::new(),
        },
    );
    pump();
    assert!(!failed_group.is_visible());
}
