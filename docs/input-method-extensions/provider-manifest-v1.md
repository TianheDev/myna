# Provider manifest version 1

An installed IM extension advertises one provider through
`/usr/share/myna/input-methods/<id>.input-method.json`. The file is data only:
Myna does not execute a command or load a library from it.

The manifest declares:

- the provider id, which must match the file name before the
  `.input-method.json` suffix;
- the display name;
- the protocol versions the provider implements;
- the fixed socket name, which must be `<id>.sock`;
- optional preedit and surrounding-text support;
- whether the provider can detect password and sensitive fields.

For example:

```json
{
  "schema_version": 1,
  "id": "example",
  "name": "Example input method",
  "protocols": [1],
  "socket": "example.sock",
  "capabilities": {
    "preedit": true,
    "surrounding_text": false,
    "secure_field_detection": true
  }
}
```

The machine-readable definition is `provider-manifest-v1.schema.json`.
`secure_field_detection` may be false in a malformed or limited installation,
but Myna Settings will show it as broken and will not allow the user to
activate it.

Myna Settings reads this host system directory. The confined daemon does not:
it receives only the selected id through shared client settings and derives
the socket path under its own runtime directory.
