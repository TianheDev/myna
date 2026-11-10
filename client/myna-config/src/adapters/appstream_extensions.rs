//! AppStream discovery for addons that extend Myna Settings.

use std::collections::BTreeSet;
use std::sync::Arc;

use xml::reader::{EventReader, XmlEvent};

use crate::command::{CancellationToken, CommandError, CommandRequest, CommandRunner};
use crate::extension_packages::MYNA_COMPONENT_ID;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppStreamExtension {
    pub component_id: String,
    pub package_name: String,
    pub name: String,
    pub summary: String,
}

pub struct AppStreamExtensionCatalog {
    runner: Arc<dyn CommandRunner>,
}

impl AppStreamExtensionCatalog {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    pub async fn discover(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<AppStreamExtension>, String> {
        let ids = self.component_ids(cancellation.clone()).await?;
        let mut extensions = Vec::new();
        for id in ids {
            if cancellation.is_cancelled() {
                return Err("AppStream discovery was cancelled".into());
            }
            let request = CommandRequest::new(
                "appstreamcli".into(),
                vec!["--no-color".into(), "dump".into(), id],
            );
            let output = self
                .runner
                .run(request, cancellation.clone())
                .await
                .map_err(command_error)?;
            if let Ok(extension) = parse_addon(output.stdout()) {
                extensions.push(extension);
            }
        }
        extensions.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.component_id.cmp(&right.component_id))
        });
        extensions.dedup_by(|left, right| left.component_id == right.component_id);
        Ok(extensions)
    }

    async fn component_ids(&self, cancellation: CancellationToken) -> Result<Vec<String>, String> {
        let request = CommandRequest::new(
            "appstreamcli".into(),
            vec![
                "--no-color".into(),
                "search".into(),
                "--details".into(),
                "Myna".into(),
            ],
        );
        let output = self
            .runner
            .run(request, cancellation)
            .await
            .map_err(command_error)?;
        let mut ids = BTreeSet::new();
        for line in output.stdout().lines() {
            let Some(identifier) = line.strip_prefix("Identifier: ") else {
                continue;
            };
            let Some(id) = identifier.split_whitespace().next() else {
                continue;
            };
            if !id.is_empty() {
                ids.insert(id.to_owned());
            }
        }
        Ok(ids.into_iter().collect())
    }
}

fn command_error(error: CommandError) -> String {
    match error {
        CommandError::NotFound { executable } => {
            format!("{executable} is not installed")
        }
        error => error.to_string(),
    }
}

fn parse_addon(xml: &str) -> Result<AppStreamExtension, String> {
    let parser = EventReader::from_str(xml);
    let mut stack = Vec::<String>::new();
    let mut addon = false;
    let mut id = String::new();
    let mut name = String::new();
    let mut summary = String::new();
    let mut package_name = String::new();
    let mut extends_myna = false;

    for event in parser {
        match event.map_err(|error| error.to_string())? {
            XmlEvent::StartElement {
                name: element,
                attributes,
                ..
            } => {
                if element.local_name == "component" {
                    addon = attributes.iter().any(|attribute| {
                        attribute.name.local_name == "type" && attribute.value == "addon"
                    });
                    id.clear();
                    name.clear();
                    summary.clear();
                    package_name.clear();
                    extends_myna = false;
                }
                stack.push(element.local_name);
            }
            XmlEvent::Characters(text) => {
                if !addon {
                    continue;
                }
                match stack.last().map(String::as_str) {
                    Some("id") if id.is_empty() => id = text.trim().to_owned(),
                    Some("name") if name.is_empty() => name = text.trim().to_owned(),
                    Some("summary") if summary.is_empty() => summary = text.trim().to_owned(),
                    Some("pkgname") if package_name.is_empty() => {
                        package_name = text.trim().to_owned();
                    }
                    Some("extends") if text.trim() == MYNA_COMPONENT_ID => extends_myna = true,
                    _ => {}
                }
            }
            XmlEvent::EndElement { name: element } => {
                if element.local_name == "component" {
                    if addon && extends_myna && !id.is_empty() && !package_name.is_empty() {
                        let display_name = if name.is_empty() {
                            package_name.clone()
                        } else {
                            name
                        };
                        return Ok(AppStreamExtension {
                            component_id: id,
                            package_name,
                            name: display_name,
                            summary,
                        });
                    }
                    addon = false;
                }
                stack.pop();
            }
            _ => {}
        }
    }
    Err("AppStream component is not a Myna addon".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{CommandOutput, FakeCommandRunner};

    fn output(text: &str) -> Result<CommandOutput, CommandError> {
        Ok(CommandOutput::new(Some(0), text, String::new()))
    }

    #[test]
    fn an_addon_is_accepted_only_when_it_extends_myna_and_names_a_package() {
        let addon = parse_addon(
            r#"<?xml version="1.0"?>
            <component type="addon">
              <id>org.example.Myna.Fcitx</id>
              <name>Fcitx input for Myna</name>
              <summary>Use Myna through Fcitx</summary>
              <extends>com.canonical.Myna.Config</extends>
              <pkgname>myna-fcitx</pkgname>
            </component>"#,
        )
        .unwrap();
        assert_eq!(
            addon,
            AppStreamExtension {
                component_id: "org.example.Myna.Fcitx".into(),
                package_name: "myna-fcitx".into(),
                name: "Fcitx input for Myna".into(),
                summary: "Use Myna through Fcitx".into(),
            }
        );

        assert!(parse_addon(
            r#"<component type="addon">
              <id>org.example.Other</id>
              <name>Other</name>
              <extends>org.example.Other.App</extends>
              <pkgname>other</pkgname>
            </component>"#
        )
        .is_err());
    }

    #[test]
    fn discovery_uses_the_appstream_database_and_dumps_matching_components() {
        let runner = FakeCommandRunner::scripted([
            output(
                "Identifier: com.canonical.Myna.Config [desktop-application]\n\
                 Identifier: org.example.Myna.Fcitx [addon]\n",
            ),
            output(
                r#"<component type="desktop-application">
                  <id>com.canonical.Myna.Config</id>
                  <name>Myna Settings</name>
                  <pkgname>myna-config</pkgname>
                </component>"#,
            ),
            output(
                r#"<component type="addon">
                  <id>org.example.Myna.Fcitx</id>
                  <name>Fcitx input for Myna</name>
                  <summary>Use Myna through Fcitx</summary>
                  <extends>com.canonical.Myna.Config</extends>
                  <pkgname>myna-fcitx</pkgname>
                </component>"#,
            ),
        ]);
        let catalog = AppStreamExtensionCatalog::new(Arc::new(runner.clone()));

        let found = gtk4::glib::MainContext::new()
            .block_on(catalog.discover(CancellationToken::new()))
            .unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].package_name, "myna-fcitx");
        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].executable(), "appstreamcli");
        assert_eq!(calls[0].arguments()[1], "search");
        assert_eq!(calls[1].arguments()[1], "dump");
    }
}
