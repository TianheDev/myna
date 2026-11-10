//! The General page's installed IM extension controls.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::extensions::{
    ExtensionActionError, ExtensionChange, ExtensionChangeCause, ExtensionLifecycle,
    ExtensionRegistry, ExtensionRestartOutcome, ExtensionSaveOutcome, ExtensionSnapshot,
    ExtensionStatus,
};
use crate::myna_settings::{MynaSettingsController, PersistenceWriter};
use crate::operation_gate::OperationCoordinator;
use crate::ports::SystemConfigurator;

pub struct ExtensionUi {
    group: adw::PreferencesGroup,
    overlay: adw::ToastOverlay,
    lifecycle: Rc<ExtensionLifecycle>,
    writer: PersistenceWriter,
    configurator: Rc<dyn SystemConfigurator>,
    refreshing: Cell<bool>,
    last_refresh_error: RefCell<Option<String>>,
    rows: RefCell<Vec<adw::SwitchRow>>,
}

impl ExtensionUi {
    pub fn new(
        group: adw::PreferencesGroup,
        overlay: adw::ToastOverlay,
        registry: Rc<dyn ExtensionRegistry>,
        controller: Rc<MynaSettingsController>,
        writer: PersistenceWriter,
        configurator: Rc<dyn SystemConfigurator>,
        operations: OperationCoordinator,
    ) -> Rc<Self> {
        let lifecycle = ExtensionLifecycle::new(registry, controller, operations);
        let ui = Rc::new(Self {
            group,
            overlay,
            lifecycle,
            writer,
            configurator,
            refreshing: Cell::new(false),
            last_refresh_error: RefCell::new(None),
            rows: RefCell::new(Vec::new()),
        });
        ui.refresh();
        ui
    }

    pub fn refresh(self: &Rc<Self>) {
        if self.lifecycle.busy() || self.refreshing.replace(true) {
            return;
        }
        for row in self.rows.borrow_mut().drain(..) {
            self.group.remove(&row);
        }

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
        if snapshot.stale_active.is_some() {
            self.clear_stale();
        }
        self.render(&snapshot);
        self.refreshing.set(false);
    }

    fn render(self: &Rc<Self>, snapshot: &ExtensionSnapshot) {
        self.group.set_visible(!snapshot.entries.is_empty());
        for entry in &snapshot.entries {
            let row = adw::SwitchRow::builder()
                .title(&entry.name)
                .subtitle(status_text(entry.status))
                .active(entry.selected)
                .sensitive(entry.can_toggle())
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
            self.group.add(&row);
            self.rows.borrow_mut().push(row);
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
                                "The selected input-method extension is no longer installed. Myna will use IBus.",
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
