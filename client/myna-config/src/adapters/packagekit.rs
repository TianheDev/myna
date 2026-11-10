//! PackageKit adapter for AppStream-discovered Myna extensions.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gio::glib;
use gio::glib::variant::ToVariant;
use gio::prelude::*;

use crate::adapters::appstream_extensions::{AppStreamExtension, AppStreamExtensionCatalog};
use crate::command::{CancellationToken, CommandRunner};
use crate::extension_packages::{
    ExtensionPackage, ExtensionPackageManager, PackageManagerError, PackageProgress,
    PackageProgressCallback,
};

const PACKAGEKIT_BUS: &str = "org.freedesktop.PackageKit";
const PACKAGEKIT_PATH: &str = "/org/freedesktop/PackageKit";
const PACKAGEKIT_INTERFACE: &str = "org.freedesktop.PackageKit";
const TRANSACTION_INTERFACE: &str = "org.freedesktop.PackageKit.Transaction";
const MANIFEST_DIRECTORY: &str = "/usr/share/myna/input-methods";
const MANIFEST_SUFFIX: &str = ".input-method.json";

pub struct PackageKitExtensionManager {
    catalog: AppStreamExtensionCatalog,
}

impl PackageKitExtensionManager {
    pub fn system(runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            catalog: AppStreamExtensionCatalog::new(runner),
        }
    }
}

#[async_trait(?Send)]
impl ExtensionPackageManager for PackageKitExtensionManager {
    async fn discover(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<ExtensionPackage>, PackageManagerError> {
        let addons = match self.catalog.discover(cancellation.clone()).await {
            Ok(addons) => addons,
            Err(_) if cancellation.is_cancelled() => {
                return Err(PackageManagerError::Cancelled);
            }
            Err(error) => return Err(PackageManagerError::Unavailable(error)),
        };
        gio::spawn_blocking(move || {
            with_packagekit_context(|| discover_packages(addons, cancellation))
        })
        .await
        .unwrap_or_else(|error| {
            Err(PackageManagerError::Unavailable(format!(
                "PackageKit discovery worker failed: {error:?}"
            )))
        })
    }

    async fn install(
        &self,
        package: &ExtensionPackage,
        cancellation: CancellationToken,
        progress: PackageProgressCallback,
    ) -> Result<(), PackageManagerError> {
        let package = package.clone();
        gio::spawn_blocking(move || {
            with_packagekit_context(|| {
                let client = PackageKitClient::new()?;
                let resolved =
                    client.resolve(&package.package_name, false, cancellation.clone())?;
                let id = resolved
                    .into_iter()
                    .find(|candidate| !candidate.installed)
                    .map(|candidate| candidate.package_id)
                    .ok_or_else(|| PackageManagerError::NotFound(package.package_name.clone()))?;
                client.install(&id, &package.name, cancellation, progress)
            })
        })
        .await
        .unwrap_or_else(|error| {
            Err(PackageManagerError::Unavailable(format!(
                "PackageKit install worker failed: {error:?}"
            )))
        })
    }

    async fn remove(
        &self,
        package: &ExtensionPackage,
        cancellation: CancellationToken,
        progress: PackageProgressCallback,
    ) -> Result<(), PackageManagerError> {
        let package = package.clone();
        gio::spawn_blocking(move || {
            with_packagekit_context(|| {
                let client = PackageKitClient::new()?;
                let resolved = client.resolve(&package.package_name, true, cancellation.clone())?;
                let id = resolved
                    .into_iter()
                    .find(|candidate| candidate.installed)
                    .map(|candidate| candidate.package_id)
                    .ok_or_else(|| PackageManagerError::NotFound(package.package_name.clone()))?;
                client.remove(&id, &package.name, cancellation, progress)
            })
        })
        .await
        .unwrap_or_else(|error| {
            Err(PackageManagerError::Unavailable(format!(
                "PackageKit removal worker failed: {error:?}"
            )))
        })
    }
}

fn with_packagekit_context<T>(
    operation: impl FnOnce() -> Result<T, PackageManagerError>,
) -> Result<T, PackageManagerError> {
    glib::MainContext::new()
        .with_thread_default(operation)
        .map_err(|_| {
            PackageManagerError::Unavailable("could not create a PackageKit worker context".into())
        })?
}

#[derive(Clone, Debug)]
struct PackageCandidate {
    package_id: String,
    installed: bool,
}

fn discover_packages(
    addons: Vec<AppStreamExtension>,
    cancellation: CancellationToken,
) -> Result<Vec<ExtensionPackage>, PackageManagerError> {
    if addons.is_empty() {
        return Ok(Vec::new());
    }
    let client = PackageKitClient::new()?;
    let mut packages = Vec::new();
    for addon in addons {
        if cancellation.is_cancelled() {
            return Err(PackageManagerError::Cancelled);
        }
        let resolved = match client.resolve(&addon.package_name, false, cancellation.clone()) {
            Ok(resolved) => resolved,
            Err(PackageManagerError::NotFound(_)) => continue,
            Err(error) => return Err(error),
        };
        let Some(installed) = resolved.iter().find(|candidate| candidate.installed) else {
            packages.push(ExtensionPackage {
                component_id: addon.component_id,
                package_name: addon.package_name,
                name: addon.name,
                summary: addon.summary,
                installed: false,
                provider_ids: Vec::new(),
            });
            continue;
        };
        let provider_ids = provider_ids(client.files(&installed.package_id)?);
        packages.push(ExtensionPackage {
            component_id: addon.component_id,
            package_name: addon.package_name,
            name: addon.name,
            summary: addon.summary,
            installed: true,
            provider_ids,
        });
    }
    Ok(packages)
}

fn provider_ids(files: Vec<String>) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for file in files {
        let path = Path::new(&file);
        if path.parent() != Some(Path::new(MANIFEST_DIRECTORY)) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(id) = name.strip_suffix(MANIFEST_SUFFIX) else {
            continue;
        };
        ids.insert(id.to_owned());
    }
    ids.into_iter().collect()
}

struct PackageKitClient {
    connection: gio::DBusConnection,
    root: gio::DBusProxy,
}

impl PackageKitClient {
    fn new() -> Result<Self, PackageManagerError> {
        let connection = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE)
            .map_err(|error| PackageManagerError::Unavailable(error.to_string()))?;
        let root = gio::DBusProxy::new_sync(
            &connection,
            gio::DBusProxyFlags::NONE,
            None,
            Some(PACKAGEKIT_BUS),
            PACKAGEKIT_PATH,
            PACKAGEKIT_INTERFACE,
            gio::Cancellable::NONE,
        )
        .map_err(|error| PackageManagerError::Unavailable(error.to_string()))?;
        Ok(Self { connection, root })
    }

    fn resolve(
        &self,
        package_name: &str,
        installed_only: bool,
        cancellation: CancellationToken,
    ) -> Result<Vec<PackageCandidate>, PackageManagerError> {
        let filter = if installed_only { 1u64 } else { 0u64 };
        let packages = vec![package_name.to_owned()];
        let result = self.transaction(
            "Resolve",
            &(filter, packages).to_variant(),
            package_name,
            cancellation,
            Arc::new(|_| {}),
        )?;
        if result.packages.is_empty() {
            return Err(PackageManagerError::NotFound(package_name.to_owned()));
        }
        Ok(result.packages)
    }

    fn files(&self, package_id: &str) -> Result<Vec<String>, PackageManagerError> {
        let ids = vec![package_id.to_owned()];
        let result = self.transaction(
            "GetFiles",
            &(ids,).to_variant(),
            package_id,
            CancellationToken::new(),
            Arc::new(|_| {}),
        )?;
        Ok(result.files)
    }

    fn install(
        &self,
        package_id: &str,
        package_name: &str,
        cancellation: CancellationToken,
        progress: PackageProgressCallback,
    ) -> Result<(), PackageManagerError> {
        let ids = vec![package_id.to_owned()];
        self.transaction(
            "InstallPackages",
            &(0u64, ids).to_variant(),
            package_name,
            cancellation,
            progress,
        )
        .map(|_| ())
    }

    fn remove(
        &self,
        package_id: &str,
        package_name: &str,
        cancellation: CancellationToken,
        progress: PackageProgressCallback,
    ) -> Result<(), PackageManagerError> {
        let ids = vec![package_id.to_owned()];
        self.transaction(
            "RemovePackages",
            &(0u64, ids, false, false).to_variant(),
            package_name,
            cancellation,
            progress,
        )
        .map(|_| ())
    }

    fn transaction(
        &self,
        method: &str,
        parameters: &glib::Variant,
        package_name: &str,
        cancellation: CancellationToken,
        progress: PackageProgressCallback,
    ) -> Result<TransactionResult, PackageManagerError> {
        let (path,): (String,) = self
            .root
            .call_sync(
                "CreateTransaction",
                None,
                gio::DBusCallFlags::NONE,
                -1,
                gio::Cancellable::NONE,
            )
            .and_then(|reply| {
                reply.get().ok_or_else(|| {
                    glib::Error::new(
                        gio::IOErrorEnum::InvalidData,
                        "CreateTransaction returned no object path",
                    )
                })
            })
            .map_err(|error| PackageManagerError::Unavailable(error.to_string()))?;
        let transaction = gio::DBusProxy::new_sync(
            &self.connection,
            gio::DBusProxyFlags::NONE,
            None,
            Some(PACKAGEKIT_BUS),
            &path,
            TRANSACTION_INTERFACE,
            gio::Cancellable::NONE,
        )
        .map_err(|error| PackageManagerError::Unavailable(error.to_string()))?;

        let state = Rc::new(RefCell::new(TransactionResult::default()));
        let cancelled = Rc::new(Cell::new(false));
        let main_loop = glib::MainLoop::new(None, false);
        let signal_handler = transaction.connect_local("g-signal", false, {
            let state = Rc::clone(&state);
            let progress = Arc::clone(&progress);
            let main_loop = main_loop.clone();
            move |values| {
                let signal = values[2].get::<String>().unwrap_or_default();
                let Ok(parameters) = values[3].get::<glib::Variant>() else {
                    return None;
                };
                let mut state = state.borrow_mut();
                match signal.as_str() {
                    "Package" => {
                        if let Some((info, package_id, _summary)) =
                            parameters.get::<(u32, String, String)>()
                        {
                            state.packages.push(PackageCandidate {
                                package_id,
                                installed: info == 1,
                            });
                        }
                    }
                    "Files" => {
                        if let Some((_package_id, files)) =
                            parameters.get::<(String, Vec<String>)>()
                        {
                            state.files = files;
                        }
                    }
                    "StatusChanged" => {
                        if let Some((status,)) = parameters.get::<(u32,)>() {
                            progress(PackageProgress::Status(status_text(status)));
                        }
                    }
                    "Percentage" => {
                        if let Some((percent,)) = parameters.get::<(u32,)>() {
                            progress(PackageProgress::Percent(percent.min(100) as u8));
                        }
                    }
                    "ErrorCode" => {
                        if let Some((code, details)) = parameters.get::<(u32, String)>() {
                            state.error = Some((code, details));
                        }
                    }
                    "Finished" => {
                        if let Some((exit, _runtime)) = parameters.get::<(u32, u32)>() {
                            state.exit = Some(exit);
                        }
                        main_loop.quit();
                    }
                    _ => {}
                }
                None
            }
        });

        transaction
            .call_sync(
                "SetHints",
                Some(&(vec!["interactive=true".to_owned()],).to_variant()),
                gio::DBusCallFlags::NONE,
                -1,
                gio::Cancellable::NONE,
            )
            .map_err(|error| PackageManagerError::Unavailable(error.to_string()))?;
        transaction
            .call_sync(
                method,
                Some(parameters),
                gio::DBusCallFlags::NONE,
                -1,
                gio::Cancellable::NONE,
            )
            .map_err(|error| {
                if cancellation.is_cancelled() {
                    PackageManagerError::Cancelled
                } else {
                    transaction_call_error(package_name, &error)
                }
            })?;

        let cancel_source = glib::timeout_add_local(Duration::from_millis(100), {
            let cancellation = cancellation.clone();
            let transaction = transaction.clone();
            let main_loop = main_loop.clone();
            let cancelled = Rc::clone(&cancelled);
            move || {
                if cancellation.is_cancelled() {
                    let _ = transaction.call_sync(
                        "Cancel",
                        None,
                        gio::DBusCallFlags::NONE,
                        -1,
                        gio::Cancellable::NONE,
                    );
                    cancelled.set(true);
                    main_loop.quit();
                    glib::ControlFlow::Break
                } else if transaction.connection().is_closed() {
                    main_loop.quit();
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            }
        });
        main_loop.run();
        cancel_source.remove();
        transaction.disconnect(signal_handler);

        if cancelled.get() || cancellation.is_cancelled() {
            return Err(PackageManagerError::Cancelled);
        }
        let mut result = Rc::try_unwrap(state)
            .map_err(|_| PackageManagerError::Failed {
                package: package_name.into(),
                detail: "PackageKit transaction state is still in use".into(),
            })?
            .into_inner();
        match result.exit {
            Some(1) => Ok(result),
            Some(3) => Err(PackageManagerError::Cancelled),
            Some(_) | None => {
                let (code, details) = result
                    .error
                    .take()
                    .unwrap_or((0, "PackageKit ended without a result".into()));
                Err(packagekit_error(package_name, code, details))
            }
        }
    }
}

#[derive(Default)]
struct TransactionResult {
    packages: Vec<PackageCandidate>,
    files: Vec<String>,
    error: Option<(u32, String)>,
    exit: Option<u32>,
}

fn transaction_call_error(package: &str, error: &glib::Error) -> PackageManagerError {
    let detail = error.message().to_owned();
    let lower = detail.to_lowercase();
    if lower.contains("cancel") {
        PackageManagerError::Cancelled
    } else if lower.contains("authoriz") || lower.contains("denied") {
        PackageManagerError::Denied {
            package: package.into(),
            detail,
        }
    } else {
        PackageManagerError::Unavailable(detail)
    }
}

fn packagekit_error(package: &str, code: u32, detail: String) -> PackageManagerError {
    let lower = detail.to_lowercase();
    if lower.contains("cancel") {
        PackageManagerError::Cancelled
    } else if lower.contains("authoriz") || lower.contains("denied") {
        PackageManagerError::Denied {
            package: package.into(),
            detail,
        }
    } else {
        PackageManagerError::Failed {
            package: package.into(),
            detail: format!("PackageKit error {code}: {detail}"),
        }
    }
}

fn status_text(status: u32) -> String {
    match status {
        1 => gettextrs::gettext("Waiting for package manager lock"),
        2 => gettextrs::gettext("Preparing package sources"),
        3 => gettextrs::gettext("Changing packages"),
        4 => gettextrs::gettext("Removing packages"),
        5 => gettextrs::gettext("Refreshing package sources"),
        6 => gettextrs::gettext("Downloading files"),
        7 => gettextrs::gettext("Installing packages"),
        8 => gettextrs::gettext("Updating packages"),
        9 => gettextrs::gettext("Cleaning up"),
        11 => gettextrs::gettext("Resolving dependencies"),
        12 => gettextrs::gettext("Checking signatures"),
        13 => gettextrs::gettext("Testing package changes"),
        14 => gettextrs::gettext("Applying package changes"),
        18 => gettextrs::gettext("Downloading repository information"),
        19 => gettextrs::gettext("Downloading package lists"),
        20 => gettextrs::gettext("Downloading file lists"),
        21 => gettextrs::gettext("Downloading changelogs"),
        22 => gettextrs::gettext("Downloading package groups"),
        23 => gettextrs::gettext("Downloading update information"),
        24 => gettextrs::gettext("Repackaging files"),
        25 => gettextrs::gettext("Decompressing files"),
        26 => gettextrs::gettext("Loading package information"),
        10 => gettextrs::gettext("Removing obsolete packages"),
        _ => gettextrs::gettext("Working with packages"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_paths_map_to_provider_ids() {
        assert_eq!(
            provider_ids(vec![
                "/usr/share/myna/input-methods/fcitx5.input-method.json".into(),
                "/usr/share/doc/package/README".into(),
                "/tmp/fcitx5.input-method.json".into(),
            ]),
            ["fcitx5"]
        );
    }

    #[test]
    fn packagekit_failures_keep_cancellation_and_authorization_distinct() {
        assert_eq!(
            packagekit_error("Alpha", 13, "transaction was cancelled".into()),
            PackageManagerError::Cancelled
        );
        assert!(matches!(
            packagekit_error("Alpha", 19, "not authorized to install software".into()),
            PackageManagerError::Denied { .. }
        ));
        assert!(matches!(
            packagekit_error("Alpha", 10, "no disk space".into()),
            PackageManagerError::Failed { .. }
        ));
    }

    #[test]
    fn every_packagekit_status_has_a_user_facing_label() {
        for (status, message) in [
            (1, "Waiting for package manager lock"),
            (2, "Preparing package sources"),
            (3, "Changing packages"),
            (4, "Removing packages"),
            (5, "Refreshing package sources"),
            (6, "Downloading files"),
            (7, "Installing packages"),
            (8, "Updating packages"),
            (9, "Cleaning up"),
            (10, "Removing obsolete packages"),
            (11, "Resolving dependencies"),
            (12, "Checking signatures"),
            (13, "Testing package changes"),
            (14, "Applying package changes"),
            (18, "Downloading repository information"),
            (19, "Downloading package lists"),
            (20, "Downloading file lists"),
            (21, "Downloading changelogs"),
            (22, "Downloading package groups"),
            (23, "Downloading update information"),
            (24, "Repackaging files"),
            (25, "Decompressing files"),
            (26, "Loading package information"),
        ] {
            assert_eq!(status_text(status), gettextrs::gettext(message));
        }
        assert_eq!(
            status_text(999),
            gettextrs::gettext("Working with packages")
        );
    }

    #[test]
    fn transaction_errors_keep_the_users_next_action_clear() {
        let error = |message| glib::Error::new(gio::IOErrorEnum::Failed, message);
        assert_eq!(
            transaction_call_error("Alpha", &error("request cancelled")),
            PackageManagerError::Cancelled
        );
        assert!(matches!(
            transaction_call_error("Alpha", &error("not authorized")),
            PackageManagerError::Denied { .. }
        ));
        assert!(matches!(
            transaction_call_error("Alpha", &error("daemon disappeared")),
            PackageManagerError::Unavailable(_)
        ));
    }
}
