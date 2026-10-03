# zbus_xml 4.0.0 compatibility patch

Upstream: [dbus2/zbus at 25876f399b068749c337374505c35e92dc9e5872](https://github.com/dbus2/zbus/tree/25876f399b068749c337374505c35e92dc9e5872/zbus_xml).
Source archive: [crates.io zbus_xml 4.0.0](https://crates.io/api/v1/crates/zbus_xml/4.0.0/download).
Archive SHA-256: `ab3f374552b954f6abb4bd6ce979e6c9b38fb9d0cd7cc68a7d796e70c9f3a233`.
The MIT license is copied from the same upstream commit's `LICENSE-MIT` file.

Changes:

- Raise quick-xml from 0.30 to 0.41 to address RUSTSEC-2026-0194 and RUSTSEC-2026-0195. There is no compatible upstream zbus_xml 4.x release with that dependency update.
- Map the newer quick-xml serialization error into the existing public `Error::QuickXml(DeError::Custom)` variant. Keep the 4.x API and accessibility dependencies.
- Omit absent optional XML attributes instead of emitting empty strings, which cannot be parsed back as optional argument directions. Keep explicitly present values unchanged.
- Strengthen the upstream serialization test with a parse/write/parse round trip, and test propagation of a synthetic writer failure. Keep the invalid argument-type fixture test.

The workspace patch applies to this library only. Rust sources otherwise retain the upstream implementation. The three tests use embedded XML fixtures and an in-memory failing writer; they do not contact D-Bus or access devices. Cargo and rustfmt may normalize formatting.
