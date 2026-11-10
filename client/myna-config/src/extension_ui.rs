//! The General page's installed IM extension controls.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::Duration;

use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::command::CancellationToken;
use crate::extension_packages::{
    ExtensionPackage, ExtensionPackageCoordinator, ExtensionPackageManager, PackageOperationError,
    PackageOperationOutcome, PackageProgress, PackageProgressCallback,
};
use crate::extensions::{
    active_id, ExtensionActionError, ExtensionChange, ExtensionChangeCause, ExtensionEntry,
    ExtensionLifecycle, ExtensionRegistry, ExtensionRestartOutcome, ExtensionSaveOutcome,
    ExtensionSnapshot, ExtensionStatus,
};
use crate::myna_settings::{MynaSettingsController, PersistenceWriter};
use crate::operation_gate::OperationCoordinator;
use crate::ports::SystemConfigurator;
use crate::ui::{installing_text, InstallControl};

pub struct ExtensionUi {
    group: adw::PreferencesGroup,
    overlay: adw::ToastOverlay,
    lifecycle: Rc<ExtensionLifecycle>,
    packages: Rc<ExtensionPackageCoordinator>,
    writer: PersistenceWriter,
    configurator: Rc<dyn SystemConfigurator>,
    refreshing: Cell<bool>,
    package_refreshing: Cell<bool>,
    last_refresh_error: RefCell<Option<String>>,
    last_package_error: RefCell<Option<String>>,
    snapshot: RefCell<Option<ExtensionSnapshot>>,
    offers: RefCell<Vec<ExtensionPackage>>,
    rows: RefCell<Vec<gtk::Widget>>,
}

pub struct ExtensionUiPorts {
    pub registry: Rc<dyn ExtensionRegistry>,
    pub package_manager: Rc<dyn ExtensionPackageManager>,
    pub controller: Rc<MynaSettingsController>,
    pub writer: PersistenceWriter,
    pub configurator: Rc<dyn SystemConfigurator>,
    pub operations: OperationCoordinator,
}

impl ExtensionUi {
    pub fn new(
        group: adw::PreferencesGroup,
        overlay: adw::ToastOverlay,
        ports: ExtensionUiPorts,
    ) -> Rc<Self> {
        let lifecycle = ExtensionLifecycle::new(
            Rc::clone(&ports.registry),
            Rc::clone(&ports.controller),
            ports.operations.clone(),
        );
        let packages = ExtensionPackageCoordinator::new(
            ports.package_manager,
            ports.registry,
            ports.controller,
            ports.writer.clone(),
            Rc::clone(&ports.configurator),
            ports.operations,
        );
        let ui = Rc::new(Self {
            group,
            overlay,
            lifecycle,
            packages,
            writer: ports.writer,
            configurator: ports.configurator,
            refreshing: Cell::new(false),
            package_refreshing: Cell::new(false),
            last_refresh_error: RefCell::new(None),
            last_package_error: RefCell::new(None),
            snapshot: RefCell::new(None),
            offers: RefCell::new(Vec::new()),
            rows: RefCell::new(Vec::new()),
        });
        ui.refresh();
        ui
    }

    pub fn refresh(self: &Rc<Self>) {
        if self.lifecycle.busy() || self.packages.busy() || self.refreshing.replace(true) {
            return;
        }
        self.clear_rows();

        let snapshot = match self.lifecycle.snapshot() {
            Ok(snapshot) => {
                self.last_refresh_error.borrow_mut().take();
                snapshot
            }
            Err(ExtensionActionError::Discovery(error)) => {
                self.group.set_visible(false);
                self.report_refresh_error(&discovery_error_text(&error));
                self.refreshing.set(false);
                return;
            }
            Err(error) => {
                self.group.set_visible(false);
                if let Some(error) = action_error_text(&error) {
                    self.report_refresh_error(&error);
                }
                self.refreshing.set(false);
                return;
            }
        };
        self.snapshot.replace(Some(snapshot.clone()));
        if snapshot.stale_active.is_some() {
            self.clear_stale();
        }
        self.render();
        self.refreshing.set(false);
        self.refresh_packages();
    }

    fn render(self: &Rc<Self>) {
        self.clear_rows();
        let Some(snapshot) = self.snapshot.borrow().clone() else {
            self.group.set_visible(false);
            return;
        };
        let packages = self.offers.borrow().clone();
        let mut entries = snapshot.entries;
        let active = active_id(&self.lifecycle.controller());
        for package in packages.iter().filter(|package| package.installed) {
            if package.provider_ids.is_empty() {
                entries.push(ExtensionEntry {
                    id: package.package_name.clone(),
                    name: package.name.clone(),
                    status: ExtensionStatus::Broken,
                    selected: false,
                });
            }
            for id in &package.provider_ids {
                if entries.iter().any(|entry| entry.id == *id) {
                    continue;
                }
                entries.push(ExtensionEntry {
                    id: id.clone(),
                    name: package.name.clone(),
                    status: ExtensionStatus::Broken,
                    selected: active.as_deref() == Some(id),
                });
            }
        }

        for entry in &entries {
            let package = package_for_provider(&packages, &entry.id);
            let row = adw::SwitchRow::builder()
                .title(&entry.name)
                .subtitle(status_text(entry.status))
                .active(entry.selected)
                .sensitive(entry.can_toggle() || package.is_some())
                .build();
            row.update_property(&[gtk::accessible::Property::Description(&status_text(
                entry.status,
            ))]);
            let updating = Rc::new(Cell::new(false));
            row.connect_active_notify({
                let ui = Rc::downgrade(self);
                let updating = Rc::clone(&updating);
                let id = entry.id.clone();
                let togglable = entry.can_toggle();
                move |row| {
                    if updating.get() {
                        return;
                    }
                    let Some(ui) = ui.upgrade() else {
                        return;
                    };
                    if row.is_active() {
                        if togglable {
                            ui.set_active(Some(id.clone()));
                        } else {
                            updating.set(true);
                            row.set_active(false);
                            updating.set(false);
                        }
                    } else {
                        ui.set_active(None);
                    }
                }
            });
            if let Some(package) = package {
                let control = InstallControl::new();
                control.show_offer(
                    &gettextrs::gettext("Remove"),
                    &gettextrs::gettext("Remove {package}").replace("{package}", &package.name),
                );
                control.button().connect_clicked({
                    let ui = Rc::downgrade(self);
                    let package = package.clone();
                    let control = control.clone();
                    move |_| {
                        if let Some(ui) = ui.upgrade() {
                            ui.run_package_operation(
                                package.clone(),
                                PackageUiAction::Remove,
                                control.clone(),
                            );
                        }
                    }
                });
                row.add_suffix(&control);
            }
            self.group.add(&row);
            self.rows.borrow_mut().push(row.upcast());
        }

        for package in packages.iter().filter(|package| !package.installed) {
            let row = adw::ActionRow::builder()
                .title(&package.name)
                .subtitle(&package.summary)
                .build();
            let control = InstallControl::new();
            control.show_offer(
                &gettextrs::gettext("Install"),
                &gettextrs::gettext("Install {package}").replace("{package}", &package.name),
            );
            control.button().connect_clicked({
                let ui = Rc::downgrade(self);
                let package = package.clone();
                let control = control.clone();
                move |_| {
                    if let Some(ui) = ui.upgrade() {
                        ui.run_package_operation(
                            package.clone(),
                            PackageUiAction::Install,
                            control.clone(),
                        );
                    }
                }
            });
            row.add_suffix(&control);
            self.group.add(&row);
            self.rows.borrow_mut().push(row.upcast());
        }

        self.group.set_visible(!self.rows.borrow().is_empty());
    }

    fn clear_rows(&self) {
        for row in self.rows.borrow_mut().drain(..) {
            self.group.remove(&row);
        }
    }

    fn clear_stale(self: &Rc<Self>) {
        let change = match self.lifecycle.begin_stale_cleanup() {
            Ok(Some(change)) => change,
            Ok(None) => return,
            Err(ExtensionActionError::Busy) => return,
            Err(error) => {
                if let Some(error) = action_error_text(&error) {
                    self.toast(&error);
                }
                return;
            }
        };
        let request = change.request();
        let lifecycle = Rc::clone(&self.lifecycle);
        let ui = Rc::downgrade(self);
        let configurator = Rc::clone(&self.configurator);
        crate::app::persist_request_then(
            self.writer.clone(),
            lifecycle_controller(&self.lifecycle),
            request,
            None,
            Some(Box::new(move |success| {
                match lifecycle.save_finished(change, success) {
                    ExtensionSaveOutcome::Restart(change) => {
                        if let Some(ui) = ui.upgrade() {
                            ui.toast(&gettextrs::gettext(
                                "The selected input-method extension no longer has a valid manifest. Myna will use IBus.",
                            ));
                        }
                        restart_change(Rc::clone(&lifecycle), configurator, change, ui);
                    }
                    ExtensionSaveOutcome::Failed(_) => {
                        if let Some(ui) = ui.upgrade() {
                            ui.toast(&gettextrs::gettext(
                                "The stale input-method extension selection could not be cleared.",
                            ));
                        }
                    }
                }
            })),
        );
    }

    fn set_active(self: &Rc<Self>, id: Option<String>) {
        let change = match self.lifecycle.begin_user_change(id.as_deref()) {
            Ok(Some(change)) => change,
            Ok(None) => {
                self.refresh();
                return;
            }
            Err(ExtensionActionError::Busy) => {
                self.refresh();
                return;
            }
            Err(error) => {
                if let Some(error) = action_error_text(&error) {
                    self.toast(&error);
                }
                self.refresh();
                return;
            }
        };
        for candidate in self.rows.borrow().iter() {
            candidate.set_sensitive(false);
        }
        let request = change.request();
        let lifecycle = Rc::clone(&self.lifecycle);
        let ui = Rc::downgrade(self);
        let configurator = Rc::clone(&self.configurator);
        crate::app::persist_request_then(
            self.writer.clone(),
            lifecycle_controller(&self.lifecycle),
            request,
            None,
            Some(Box::new(move |success| {
                match lifecycle.save_finished(change, success) {
                    ExtensionSaveOutcome::Restart(change) => {
                        restart_change(Rc::clone(&lifecycle), configurator, change, ui);
                    }
                    ExtensionSaveOutcome::Failed(ExtensionChangeCause::User) => {
                        if let Some(ui) = ui.upgrade() {
                            ui.toast(&gettextrs::gettext(
                                "The input-method extension selection could not be saved.",
                            ));
                            ui.schedule_refresh();
                        }
                    }
                    ExtensionSaveOutcome::Failed(ExtensionChangeCause::StaleSelection) => {
                        if let Some(ui) = ui.upgrade() {
                            ui.schedule_refresh();
                        }
                    }
                }
            })),
        );
    }

    fn refresh_packages(self: &Rc<Self>) {
        if self.package_refreshing.get() || self.packages.busy() {
            return;
        }
        self.package_refreshing.set(true);
        let ui = Rc::downgrade(self);
        let packages = Rc::clone(&self.packages);
        glib::spawn_future_local(async move {
            let result = packages.discover(CancellationToken::new()).await;
            let Some(ui) = ui.upgrade() else {
                return;
            };
            ui.package_refreshing.set(false);
            match result {
                Ok(offers) => {
                    ui.last_package_error.borrow_mut().take();
                    ui.offers.replace(offers);
                    if !ui.lifecycle.busy() && !ui.packages.busy() {
                        ui.render();
                    }
                }
                Err(error) => {
                    let message = gettextrs::gettext(
                        "Installable input-method extensions could not be read: {detail}",
                    )
                    .replace("{detail}", &error.to_string());
                    ui.report_package_error(&message);
                }
            }
        });
    }

    fn run_package_operation(
        self: &Rc<Self>,
        package: ExtensionPackage,
        action: PackageUiAction,
        control: InstallControl,
    ) {
        if self.packages.busy() {
            return;
        }
        control.show_busy(&action.starting_text());
        let (progress, done) = package_progress(control.clone(), action);
        let coordinator = Rc::clone(&self.packages);
        let ui = Rc::downgrade(self);
        let package_name = package.name.clone();
        glib::spawn_future_local(async move {
            let result = match action {
                PackageUiAction::Install => {
                    coordinator
                        .install(&package, CancellationToken::new(), progress)
                        .await
                }
                PackageUiAction::Remove => {
                    coordinator
                        .remove(&package, CancellationToken::new(), progress)
                        .await
                }
            };
            done.store(true, Ordering::Release);
            let Some(ui) = ui.upgrade() else {
                return;
            };
            match result {
                Ok(PackageOperationOutcome::Installed) => {
                    control.show_installed();
                }
                Ok(
                    PackageOperationOutcome::Removed
                    | PackageOperationOutcome::RemovedWithoutActiveProvider,
                ) => {}
                Err(PackageOperationError::Busy) => {
                    action.restore(&control, &package_name);
                }
                Err(error) => {
                    action.restore(&control, &package_name);
                    ui.toast(
                        &gettextrs::gettext(
                            "The input-method extension package could not be changed: {detail}",
                        )
                        .replace("{detail}", &error.to_string()),
                    );
                }
            }
            ui.schedule_refresh();
        });
    }

    fn schedule_refresh(self: &Rc<Self>) {
        let ui = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(ui) = ui.upgrade() {
                ui.refresh();
            }
        });
    }

    fn toast(&self, message: &str) {
        self.overlay.add_toast(adw::Toast::new(message));
    }

    fn report_refresh_error(&self, message: &str) {
        let mut last = self.last_refresh_error.borrow_mut();
        if last.as_deref() != Some(message) {
            self.toast(message);
            *last = Some(message.to_owned());
        }
    }

    fn report_package_error(&self, message: &str) {
        let mut last = self.last_package_error.borrow_mut();
        if last.as_deref() != Some(message) {
            self.toast(message);
            *last = Some(message.to_owned());
        }
    }
}

fn lifecycle_controller(lifecycle: &ExtensionLifecycle) -> Rc<MynaSettingsController> {
    lifecycle.controller()
}

fn restart_change(
    lifecycle: Rc<ExtensionLifecycle>,
    configurator: Rc<dyn SystemConfigurator>,
    change: ExtensionChange,
    ui: Weak<ExtensionUi>,
) {
    glib::spawn_future_local(async move {
        if let ExtensionRestartOutcome::Failed(error) =
            lifecycle.restart(change, configurator.as_ref()).await
        {
            if let Some(ui) = ui.upgrade() {
                ui.toast(
                    &gettextrs::gettext(
                        "Myna could not restart. The input-method selection will apply after Myna restarts: {detail}",
                    )
                    .replace("{detail}", &error),
                );
            }
        }
        if let Some(ui) = ui.upgrade() {
            ui.schedule_refresh();
        }
    });
}

#[derive(Clone, Copy)]
enum PackageUiAction {
    Install,
    Remove,
}

impl PackageUiAction {
    fn starting_text(self) -> String {
        match self {
            Self::Install => gettextrs::gettext("Installing…"),
            Self::Remove => gettextrs::gettext("Removing…"),
        }
    }

    fn progress_text(self, progress: &PackageProgress) -> String {
        match progress {
            PackageProgress::Status(status) => status.clone(),
            PackageProgress::Percent(percent) => match self {
                Self::Install => installing_text(Some(*percent)),
                Self::Remove => gettextrs::gettext("Removing {percent}%")
                    .replace("{percent}", &percent.to_string()),
            },
        }
    }

    fn restore(self, control: &InstallControl, package: &str) {
        let label = match self {
            Self::Install => gettextrs::gettext("Install"),
            Self::Remove => gettextrs::gettext("Remove"),
        };
        let accessible = match self {
            Self::Install => gettextrs::gettext("Install {package}"),
            Self::Remove => gettextrs::gettext("Remove {package}"),
        }
        .replace("{package}", package);
        control.show_offer(&label, &accessible);
    }
}

fn package_progress(
    control: InstallControl,
    action: PackageUiAction,
) -> (PackageProgressCallback, Arc<AtomicBool>) {
    let (sender, receiver) = mpsc::channel();
    let done = Arc::new(AtomicBool::new(false));
    let finished = Arc::clone(&done);
    glib::timeout_add_local(Duration::from_millis(100), move || {
        while let Ok(progress) = receiver.try_recv() {
            control.show_busy(&action.progress_text(&progress));
        }
        if finished.load(Ordering::Acquire) {
            while let Ok(progress) = receiver.try_recv() {
                control.show_busy(&action.progress_text(&progress));
            }
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
    let callback: PackageProgressCallback = Arc::new(move |progress| {
        let _ = sender.send(progress);
    });
    (callback, done)
}

fn package_for_provider<'a>(
    packages: &'a [ExtensionPackage],
    provider_id: &str,
) -> Option<&'a ExtensionPackage> {
    packages.iter().find(|package| {
        package.installed
            && (package
                .provider_ids
                .iter()
                .any(|candidate| candidate == provider_id)
                || (package.provider_ids.is_empty() && package.package_name == provider_id))
    })
}

fn status_text(status: ExtensionStatus) -> String {
    match status {
        ExtensionStatus::Enabled => gettextrs::gettext("Enabled"),
        ExtensionStatus::Disabled => gettextrs::gettext("Disabled"),
        ExtensionStatus::Unavailable => {
            gettextrs::gettext("The provider cannot be reached right now.")
        }
        ExtensionStatus::Broken => {
            gettextrs::gettext("This extension is broken and cannot be enabled.")
        }
    }
}

fn action_error_text(error: &ExtensionActionError) -> Option<String> {
    match error {
        ExtensionActionError::Busy => None,
        ExtensionActionError::InvalidSelection => Some(gettextrs::gettext(
            "This input-method extension cannot be enabled.",
        )),
        ExtensionActionError::Discovery(detail) => Some(discovery_error_text(detail)),
        ExtensionActionError::Save(detail) => Some(
            gettextrs::gettext("The input-method extension selection could not be saved: {detail}")
                .replace("{detail}", detail),
        ),
    }
}

fn discovery_error_text(detail: &str) -> String {
    gettextrs::gettext("Installed input-method extensions could not be read: {detail}")
        .replace("{detail}", detail)
}
