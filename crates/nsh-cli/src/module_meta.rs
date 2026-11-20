//! What the app's crates and nshm need of the packaged app: `Info.plist`
//! keys and entitlements on iOS, permissions and manifest entries on
//! Android, which a crate declares in its `Cargo.toml`:
//!
//! ```toml
//! [package.metadata.nsh.ios.info]           # always, when the crate is linked
//! NSExampleUsageDescription = "..."
//!
//! [package.metadata.nsh.features.camera.ios.info]   # when `camera` is on
//! NSCameraUsageDescription = "The app takes photos with the camera."
//!
//! [package.metadata.nsh.features.camera.android]
//! permissions = ["android.permission.CAMERA"]
//! queries = ['<intent><action android:name="android.media.action.IMAGE_CAPTURE"/></intent>']
//! application = ['<service android:name="..." android:exported="false"/>']
//! ```
//!
//! Every package in the generated workspace's dependency graph is read, so
//! a crate the app's own crate depends on (by path, Git or crates.io) counts
//! as an nshm `nsh add` lists does, with the features Cargo turns on for it.
//! The entries are merged ([`ModuleMeta::merge`]): a key two crates set to
//! different values is an error naming both, arrays are joined without
//! repeats. The app's own `nsh.json` wins over them ([`ModuleMeta::apply`]).
//!
//! `ios.info`, `ios.entitlements`, `android.permissions`, `android.queries`
//! (`<intent>`, `<package>` and `<provider>` elements) and
//! `android.application` (`<service>`, `<receiver>`, `<provider>`,
//! `<activity>` and `<meta-data>` elements) are read; other keys are
//! reported and left out.

use std::{collections::BTreeMap, path::Path, process::Command};

use anyhow::{Context as _, bail};
use serde_json::{Map, Value};

use crate::{package::AppConfig, ui};

/// The elements `android.queries` takes.
const QUERY_ELEMENTS: &[&str] = &["intent", "package", "provider"];

/// The elements `android.application` takes: components and their
/// metadata, never the `<application>` element's own attributes.
const APPLICATION_ELEMENTS: &[&str] = &["service", "receiver", "provider", "activity", "meta-data"];

/// What the crates of an app declare for its packages.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct ModuleMeta {
    /// `Info.plist` keys of iOS packages.
    pub(crate) ios_info: Map<String, Value>,
    /// Entitlements of iOS packages.
    pub(crate) ios_entitlements: Map<String, Value>,
    /// Android permissions (`<uses-permission>`).
    pub(crate) android_permissions: Vec<String>,
    /// Elements of the manifest's `<queries>`.
    pub(crate) android_queries: Vec<String>,
    /// Elements of the manifest's `<application>`.
    pub(crate) android_application: Vec<String>,
    /// Which package set each key, for errors.
    owners: BTreeMap<String, String>,
}

impl ModuleMeta {
    /// What the packages of the generated workspace `manifest` declare, as
    /// Cargo resolves it for `triple` (`None`: every platform).
    pub(crate) fn of_workspace(manifest: &Path, triple: Option<&str>) -> anyhow::Result<Self> {
        let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        cmd.args(["metadata", "--format-version", "1", "--manifest-path"])
            .arg(manifest);
        if let Some(triple) = triple {
            cmd.args(["--filter-platform", triple]);
        }
        let out = cmd
            .output()
            .context("running cargo metadata for the app's crates")?;
        if !out.status.success() {
            bail!(
                "cargo metadata failed for the app's crates:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let json: Value =
            serde_json::from_slice(&out.stdout).context("cargo metadata printed no JSON")?;
        let (meta, warnings) = Self::from_metadata(&json)?;
        for warning in warnings {
            ui::warn(warning);
        }
        Ok(meta)
    }

    /// What `cargo metadata`'s output `json` declares: each resolved
    /// package's `[package.metadata.nsh]`, its sections for the features
    /// Cargo turned on; and the warnings about what was left out.
    pub(crate) fn from_metadata(json: &Value) -> anyhow::Result<(Self, Vec<String>)> {
        let packages: BTreeMap<&str, &Value> = json["packages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some((p["id"].as_str()?, p)))
            .collect();
        let mut meta = ModuleMeta::default();
        let mut warnings = Vec::new();
        let mut nodes: Vec<&Value> = json["resolve"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .collect();
        // One order whatever Cargo printed: errors and arrays stay stable.
        nodes.sort_by_key(|n| n["id"].as_str().unwrap_or_default());
        for node in nodes {
            let Some(package) = node["id"].as_str().and_then(|id| packages.get(id)) else {
                continue;
            };
            let Some(nsh) = package["metadata"]["nsh"].as_object() else {
                continue;
            };
            let name = package["name"].as_str().unwrap_or("?");
            meta.merge_section(name, "[package.metadata.nsh]", nsh, &mut warnings)?;
            let features = nsh.get("features").and_then(Value::as_object);
            let enabled = node["features"].as_array().into_iter().flatten();
            for feature in enabled.filter_map(Value::as_str) {
                if let Some(section) = features
                    .and_then(|f| f.get(feature))
                    .and_then(Value::as_object)
                {
                    let at = format!("[package.metadata.nsh.features.{feature}]");
                    meta.merge_section(name, &at, section, &mut warnings)?;
                }
            }
        }
        Ok((meta, warnings))
    }

    /// Merges one section (`at`, of package `name`): its `ios` and
    /// `android` tables.
    fn merge_section(
        &mut self,
        name: &str,
        at: &str,
        section: &Map<String, Value>,
        warnings: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        for (key, value) in section {
            match key.as_str() {
                // Read by itself: a feature's section is merged only when on.
                "features" if at == "[package.metadata.nsh]" => {}
                "ios" => self.merge_ios(name, at, value, warnings)?,
                "android" => self.merge_android(name, at, value, warnings)?,
                _ => warnings.push(format!(
                    "{name}: {at} `{key}` is not something this nsh reads: left out"
                )),
            }
        }
        Ok(())
    }

    fn merge_ios(
        &mut self,
        name: &str,
        at: &str,
        ios: &Value,
        warnings: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        let ios = ios
            .as_object()
            .with_context(|| format!("{name}: {at} `ios` is a table"))?;
        for (key, value) in ios {
            let (target, kind) = match key.as_str() {
                "info" => (&mut self.ios_info, "ios.info"),
                "entitlements" => (&mut self.ios_entitlements, "ios.entitlements"),
                _ => {
                    warnings.push(format!(
                        "{name}: {at} `ios.{key}` is not something this nsh reads: left out"
                    ));
                    continue;
                }
            };
            let entries = value
                .as_object()
                .with_context(|| format!("{name}: {at} `{kind}` is a table"))?;
            for (entry, value) in entries {
                if kind == "ios.info" && crate::apple::is_reserved_ios_info(entry) {
                    bail!("{name}: {at} `ios.info.{entry}` is a key NSH sets itself");
                }
                let owner = format!("{kind}.{entry}");
                match target.get_mut(entry) {
                    None => {
                        target.insert(entry.clone(), value.clone());
                        self.owners.insert(owner, name.to_owned());
                    }
                    Some(Value::Array(have)) if value.is_array() => {
                        join(have, value.as_array().into_iter().flatten().cloned());
                    }
                    Some(have) if have == value => {}
                    Some(_) => {
                        let other = self.owners.get(&owner).map_or("another crate", |o| o);
                        bail!(
                            "{name} and {other} set `{entry}` ({kind}) to different values: \
                             set it in the app's nsh.json instead"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    fn merge_android(
        &mut self,
        name: &str,
        at: &str,
        android: &Value,
        warnings: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        let android = android
            .as_object()
            .with_context(|| format!("{name}: {at} `android` is a table"))?;
        for (key, value) in android {
            let strings = || -> anyhow::Result<Vec<String>> {
                value
                    .as_array()
                    .and_then(|list| {
                        list.iter()
                            .map(|v| v.as_str().map(str::to_owned))
                            .collect::<Option<Vec<_>>>()
                    })
                    .with_context(|| format!("{name}: {at} `android.{key}` is a list of strings"))
            };
            match key.as_str() {
                "permissions" => join_strings(&mut self.android_permissions, strings()?),
                "queries" => {
                    let elements = strings()?;
                    for e in &elements {
                        check_element(e, QUERY_ELEMENTS)
                            .with_context(|| format!("{name}: {at} `android.queries`"))?;
                    }
                    join_strings(&mut self.android_queries, elements);
                }
                "application" => {
                    let elements = strings()?;
                    for e in &elements {
                        check_element(e, APPLICATION_ELEMENTS)
                            .with_context(|| format!("{name}: {at} `android.application`"))?;
                    }
                    join_strings(&mut self.android_application, elements);
                }
                _ => warnings.push(format!(
                    "{name}: {at} `android.{key}` is not something this nsh reads: left out"
                )),
            }
        }
        Ok(())
    }

    /// Whether nothing is declared.
    pub(crate) fn is_empty(&self) -> bool {
        self.ios_info.is_empty()
            && self.ios_entitlements.is_empty()
            && self.android_permissions.is_empty()
            && self.android_queries.is_empty()
            && self.android_application.is_empty()
    }

    /// Adds what the crates declare to the app's `config` (its `nsh.json`):
    /// a key the app sets keeps the app's value, its arrays joined with
    /// the crates' entries; permissions are joined.
    pub(crate) fn apply(&self, config: &mut AppConfig) {
        under(&mut config.ios.info_plist, &self.ios_info);
        under(&mut config.ios.entitlements, &self.ios_entitlements);
        join_strings(
            &mut config.android.permissions,
            self.android_permissions.clone(),
        );
        join_strings(&mut config.android.queries, self.android_queries.clone());
        join_strings(
            &mut config.android.application,
            self.android_application.clone(),
        );
    }
}

/// What the current app's crates declare for a development host built for
/// `triple`: nothing when the app has no crates; a failure is reported and
/// nothing taken, so the session still runs.
pub(crate) fn for_dev_host(triple: &str) -> ModuleMeta {
    let Some(app) = crate::rust_app::current() else {
        return ModuleMeta::default();
    };
    ModuleMeta::of_workspace(&app.workspace().join("Cargo.toml"), Some(triple)).unwrap_or_else(
        |e| {
            ui::warn(format!(
                "the development host leaves out what the app's crates declare: {e:#}"
            ));
            ModuleMeta::default()
        },
    )
}

/// Adds `from` to `app`: the app's value wins, arrays are joined.
pub(crate) fn under(app: &mut Map<String, Value>, from: &Map<String, Value>) {
    for (key, value) in from {
        match app.get_mut(key) {
            None => {
                app.insert(key.clone(), value.clone());
            }
            Some(Value::Array(have)) => {
                join(have, value.as_array().into_iter().flatten().cloned());
            }
            Some(_) => {}
        }
    }
}

/// Appends each of `more` that `list` does not have yet.
fn join(list: &mut Vec<Value>, more: impl IntoIterator<Item = Value>) {
    for value in more {
        if !list.contains(&value) {
            list.push(value);
        }
    }
}

fn join_strings(list: &mut Vec<String>, more: Vec<String>) {
    for value in more {
        if !list.contains(&value) {
            list.push(value);
        }
    }
}

/// Checks `xml` is one of the `elements`, as written: `<name` then a space,
/// `/` or `>`, ending with `>`.
fn check_element(xml: &str, elements: &[&str]) -> anyhow::Result<()> {
    let xml = xml.trim();
    let name = xml
        .strip_prefix('<')
        .map(|rest| {
            rest.split(|c: char| c.is_whitespace() || c == '/' || c == '>')
                .next()
                .unwrap_or_default()
        })
        .unwrap_or_default();
    if !xml.ends_with('>') || !elements.contains(&name) {
        bail!(
            "{xml:?} is not one of the elements it takes: {}",
            elements
                .iter()
                .map(|e| format!("<{e}>"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// `cargo metadata`'s output for `packages` (name, metadata, features
    /// on).
    fn metadata(packages: &[(&str, Value, &[&str])]) -> Value {
        let id = |name: &str| format!("path+file:///work/{name}#0.1.0");
        json!({
            "packages": packages.iter().map(|(name, metadata, _)| json!({
                "id": id(name),
                "name": name,
                "metadata": metadata,
            })).collect::<Vec<_>>(),
            "resolve": { "nodes": packages.iter().map(|(name, _, features)| json!({
                "id": id(name),
                "features": features,
            })).collect::<Vec<_>>() },
        })
    }

    /// A crate of modules behind features, as nsh-extra declares them.
    fn extra() -> Value {
        json!({ "nsh": { "features": { "camera": {
            "ios": { "info": { "NSCameraUsageDescription": "The app takes photos with the camera." } },
            "android": { "queries": [
                "<intent><action android:name=\"android.media.action.IMAGE_CAPTURE\"/></intent>"
            ] },
        } } } })
    }

    #[test]
    fn only_the_features_on_count() {
        // The app's crate depends on nsh-extra (by path, Git or crates.io:
        // the graph is the same) with `camera` on.
        let on = metadata(&[
            ("app-native", Value::Null, &[]),
            ("nsh-extra", extra(), &["camera", "default"]),
        ]);
        let (meta, warnings) = ModuleMeta::from_metadata(&on).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(
            meta.ios_info["NSCameraUsageDescription"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );
        assert_eq!(meta.android_queries.len(), 1);
        assert!(meta.android_queries[0].contains("IMAGE_CAPTURE"));
        // Linked without the feature: nothing of the camera's.
        let off = metadata(&[("nsh-extra", extra(), &[])]);
        let (meta, _) = ModuleMeta::from_metadata(&off).unwrap();
        assert!(meta.is_empty(), "{meta:?}");
    }

    #[test]
    fn crates_merge_and_clash() {
        let a = json!({ "nsh": {
            "ios": { "info": { "UIBackgroundModes": ["remote-notification"], "A": "1" } },
            "android": { "permissions": ["android.permission.CAMERA"] },
        } });
        let b = json!({ "nsh": {
            "ios": {
                "info": { "UIBackgroundModes": ["remote-notification", "audio"], "A": "1" },
                "entitlements": { "aps-environment": "development" },
            },
            "android": {
                "permissions": ["android.permission.CAMERA", "android.permission.POST_NOTIFICATIONS"],
                "application": ["<service android:name=\"x.Push\" android:exported=\"false\"/>"],
            },
        } });
        let (meta, _) =
            ModuleMeta::from_metadata(&metadata(&[("a", a.clone(), &[]), ("b", b, &[])])).unwrap();
        assert_eq!(
            meta.ios_info["UIBackgroundModes"],
            json!(["remote-notification", "audio"])
        );
        assert_eq!(meta.ios_entitlements["aps-environment"], "development");
        assert_eq!(
            meta.android_permissions,
            [
                "android.permission.CAMERA",
                "android.permission.POST_NOTIFICATIONS"
            ]
        );
        assert_eq!(meta.android_application.len(), 1);
        // Two values of one key: an error naming both crates.
        let c = json!({ "nsh": { "ios": { "info": { "A": "2" } } } });
        let error = ModuleMeta::from_metadata(&metadata(&[("a", a, &[]), ("c", c, &[])]))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("`A`") && error.contains('a') && error.contains('c'),
            "{error}"
        );
    }

    #[test]
    fn elements_and_keys_are_checked() {
        let bad = |android: Value| {
            ModuleMeta::from_metadata(&metadata(&[(
                "x",
                json!({ "nsh": { "android": android } }),
                &[],
            )]))
        };
        // `<application>`'s own attributes are not the crates'.
        assert!(bad(json!({ "application": ["<application android:name=\"x.App\"/>"] })).is_err());
        assert!(bad(json!({ "queries": ["<service android:name=\"x\"/>"] })).is_err());
        assert!(
            bad(json!({ "application": ["<meta-data android:name=\"a\" android:value=\"b\"/>"] }))
                .is_ok()
        );
        assert!(bad(json!({ "queries": ["<package android:name=\"com.example\" />"] })).is_ok());
        assert!(bad(json!({ "permissions": "CAMERA" })).is_err());
        let reserved = json!({ "nsh": { "ios": { "info": { "CFBundleIdentifier": "x" } } } });
        assert!(ModuleMeta::from_metadata(&metadata(&[("x", reserved, &[])])).is_err());
        // Keys a later nsh reads: reported, left out.
        let (meta, warnings) = ModuleMeta::from_metadata(&metadata(&[(
            "x",
            json!({ "nsh": { "android": { "dex": ["a.dex"] }, "ios": { "privacy": {} } } }),
            &[],
        )]))
        .unwrap();
        assert!(meta.is_empty());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    #[test]
    fn the_app_wins() {
        let mut config = AppConfig::default();
        config
            .ios
            .info_plist
            .insert("NSCameraUsageDescription".into(), json!("Scan receipts."));
        config
            .ios
            .info_plist
            .insert("UIBackgroundModes".into(), json!(["audio"]));
        config.android.permissions.push("CAMERA".into());
        let mut meta = ModuleMeta::default();
        meta.ios_info
            .insert("NSCameraUsageDescription".into(), json!("Takes photos."));
        meta.ios_info
            .insert("UIBackgroundModes".into(), json!(["remote-notification"]));
        meta.ios_entitlements
            .insert("aps-environment".into(), json!("development"));
        meta.android_permissions
            .push("android.permission.POST_NOTIFICATIONS".into());
        meta.android_queries
            .push("<package android:name=\"a\"/>".into());
        meta.apply(&mut config);
        assert_eq!(
            config.ios.info_plist["NSCameraUsageDescription"],
            "Scan receipts."
        );
        assert_eq!(
            config.ios.info_plist["UIBackgroundModes"],
            json!(["audio", "remote-notification"])
        );
        assert_eq!(config.ios.entitlements["aps-environment"], "development");
        assert_eq!(
            config.android.permissions,
            ["CAMERA", "android.permission.POST_NOTIFICATIONS"]
        );
        assert_eq!(config.android.queries.len(), 1);
    }
}
