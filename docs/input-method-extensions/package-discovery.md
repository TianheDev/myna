# IM extension package discovery

Myna Settings offers an IM extension as installable only when configured
AppStream software sources advertise an `addon` component that:

- extends `com.canonical.Myna.Config`;
- has a component id and display name;
- names one package in `<pkgname>`; and
- mentions Myna in searchable metadata used by `appstreamcli search Myna`.

For example:

```xml
<component type="addon">
  <id>org.example.Myna.Fcitx</id>
  <name>Fcitx input for Myna</name>
  <summary>Use Myna through Fcitx</summary>
  <extends>com.canonical.Myna.Config</extends>
  <pkgname>myna-fcitx</pkgname>
</component>
```

The package installs its provider manifest under
`/usr/share/myna/input-methods`. Myna Settings resolves the AppStream package
through PackageKit and uses PackageKit transactions from the user's configured
software sources. It has no private extension index, URL installer, or direct
APT command path.

Installing a package does not enable its provider. Removing the active
provider first switches Myna back to IBus and restarts the daemon; the package
is removed only after that succeeds. If removal fails or is cancelled while
the provider is still available, Myna restores the previous selection.
PackageKit is asked to remove only the extension package, never the input
method framework or its language engines.
