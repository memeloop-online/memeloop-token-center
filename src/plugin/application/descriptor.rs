//! Pure distribution data, never installation approval or proof of asset readiness.
//! Protected callers must supply authority and verify signed packages and runtime receipts.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize, de};
use serde_json::Value;

use super::{PreinstalledInventory, validate_inventory_id};
use crate::{
    error::AppError,
    plugin::{PluginPackageIdentity, plugin_configuration_schema_digest, safe_plugin_token},
};

const FORMAT_VERSION: u8 = 1;
const MAX_DESCRIPTOR_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginInventoryDescriptor {
    pub format_version: u8,
    pub inventories: BTreeMap<String, DescriptorInventory>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DescriptorInventory {
    pub inventory: PreinstalledInventory,
    pub packages: BTreeMap<String, PluginPackageIdentity>,
    pub identity_digest: String,
    pub contract_digest: String,
}

impl PluginInventoryDescriptor {
    pub const CURRENT_FORMAT_VERSION: u8 = FORMAT_VERSION;
    pub const MAX_PARSE_BYTES: usize = MAX_DESCRIPTOR_BYTES;

    pub fn validate(&self) -> Result<(), AppError> {
        if self.format_version != FORMAT_VERSION {
            return Err(AppError::Forbidden);
        }
        let mut roots = BTreeSet::new();
        for (inventory_id, entry) in &self.inventories {
            validate_inventory_id(inventory_id)?;
            let root = &entry.inventory.root;
            if !root.is_absolute()
                || root.parent().is_none()
                || root
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
                || !roots.insert(root)
            {
                return Err(AppError::Forbidden);
            }
            entry.package_references()?;
        }
        let bytes = serde_json::to_vec(self).map_err(|_| AppError::Internal)?;
        if bytes.len() > MAX_DESCRIPTOR_BYTES {
            return Err(AppError::Forbidden);
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String, AppError> {
        self.validate()?;
        value_digest(self)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, AppError> {
        if bytes.len() > MAX_DESCRIPTOR_BYTES {
            return Err(AppError::Forbidden);
        }
        let mut deserializer = serde_json::Deserializer::from_slice(bytes);
        let value = UniqueValue::deserialize(&mut deserializer).map_err(|_| AppError::Forbidden)?;
        deserializer.end().map_err(|_| AppError::Forbidden)?;
        let descriptor: Self = serde_json::from_value(value.0).map_err(|_| AppError::Forbidden)?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub fn parse_expected(bytes: &[u8], expected_digest: &str) -> Result<Self, AppError> {
        let descriptor = Self::parse(bytes)?;
        if descriptor.digest()? != expected_digest {
            return Err(AppError::Forbidden);
        }
        Ok(descriptor)
    }

    pub fn validate_extension_of(&self, previous: &Self) -> Result<(), AppError> {
        self.validate()?;
        previous.validate()?;
        for (inventory_id, old) in &previous.inventories {
            let incoming = self
                .inventories
                .get(inventory_id)
                .ok_or(AppError::Forbidden)?;
            if value_digest(incoming)? != value_digest(old)? {
                return Err(AppError::Forbidden);
            }
        }
        Ok(())
    }

    pub fn validate_retains_inventory(
        &self,
        previous: &BTreeMap<String, PreinstalledInventory>,
    ) -> Result<(), AppError> {
        self.validate()?;
        for (inventory_id, old) in previous {
            let incoming = self
                .inventories
                .get(inventory_id)
                .ok_or(AppError::Forbidden)?;
            if value_digest(&incoming.inventory)? != value_digest(old)? {
                return Err(AppError::Forbidden);
            }
        }
        Ok(())
    }
}

impl DescriptorInventory {
    pub fn package_references(&self) -> Result<BTreeMap<String, String>, AppError> {
        if !valid_receipt_digest(&self.identity_digest)
            || !valid_receipt_digest(&self.contract_digest)
            || self.packages.len() != self.inventory.grants.len()
        {
            return Err(AppError::Forbidden);
        }
        let mut references = BTreeMap::new();
        let mut unique = BTreeSet::new();
        for (plugin_id, identity) in &self.packages {
            let grants = self
                .inventory
                .grants
                .get(plugin_id)
                .ok_or(AppError::Forbidden)?;
            if !safe_plugin_token(plugin_id, super::super::MAX_PLUGIN_ID_BYTES)
                || super::super::CORE_PLUGIN_KV_NAMESPACES.contains(&plugin_id.as_str())
                || grants.iter().any(|grant| {
                    !valid_receipt_digest(&grant.manifest_digest) || grant.version.is_empty()
                })
                || !grants.iter().any(|grant| &grant.identity == identity)
                || identity
                    .component_sha256
                    .as_ref()
                    .is_some_and(|digest| !valid_sha256(digest))
            {
                return Err(AppError::Forbidden);
            }
            let provenance = identity.provenance.as_ref().ok_or(AppError::Forbidden)?;
            if provenance.format_version != 1
                || !matches!(
                    provenance.signature_policy.as_str(),
                    "cosign-public-key" | "cosign-keyless"
                )
                || !valid_sha256(&provenance.digest)
                || provenance.source.is_empty()
                || provenance.source.contains('@')
                || provenance.source.contains("://")
                || provenance.source.contains(['?', '#'])
                || provenance
                    .source
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
            {
                return Err(AppError::Forbidden);
            }
            let reference = format!("{}@{}", provenance.source, provenance.digest);
            if reference.len() > 2048 || !unique.insert(reference.clone()) {
                return Err(AppError::Forbidden);
            }
            references.insert(plugin_id.clone(), reference);
        }
        Ok(references)
    }
}

fn value_digest(value: &impl Serialize) -> Result<String, AppError> {
    plugin_configuration_schema_digest(
        &serde_json::to_value(value).map_err(|_| AppError::Internal)?,
    )
}

fn valid_receipt_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_sha256(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|digest| valid_receipt_digest(digest))
}

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<Deserializer: de::Deserializer<'de>>(
        deserializer: Deserializer,
    ) -> Result<Self, Deserializer::Error> {
        struct UniqueVisitor;

        impl<'de> de::Visitor<'de> for UniqueVisitor {
            type Value = UniqueValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("JSON without duplicate object keys")
            }

            fn visit_map<Map: de::MapAccess<'de>>(
                self,
                mut map: Map,
            ) -> Result<Self::Value, Map::Error> {
                let mut object = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if object.contains_key(&key) {
                        return Err(de::Error::custom("duplicate JSON object key"));
                    }
                    object.insert(key, map.next_value::<UniqueValue>()?.0);
                }
                Ok(UniqueValue(Value::Object(object)))
            }

            fn visit_seq<Sequence: de::SeqAccess<'de>>(
                self,
                mut sequence: Sequence,
            ) -> Result<Self::Value, Sequence::Error> {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<UniqueValue>()? {
                    values.push(value.0);
                }
                Ok(UniqueValue(Value::Array(values)))
            }

            fn visit_bool<Error: de::Error>(self, value: bool) -> Result<Self::Value, Error> {
                Ok(UniqueValue(Value::Bool(value)))
            }

            fn visit_i64<Error: de::Error>(self, value: i64) -> Result<Self::Value, Error> {
                Ok(UniqueValue(Value::Number(value.into())))
            }

            fn visit_u64<Error: de::Error>(self, value: u64) -> Result<Self::Value, Error> {
                Ok(UniqueValue(Value::Number(value.into())))
            }

            fn visit_f64<Error: de::Error>(self, value: f64) -> Result<Self::Value, Error> {
                serde_json::Number::from_f64(value)
                    .map(|number| UniqueValue(Value::Number(number)))
                    .ok_or_else(|| de::Error::custom("invalid JSON number"))
            }

            fn visit_str<Error: de::Error>(self, value: &str) -> Result<Self::Value, Error> {
                Ok(UniqueValue(Value::String(value.to_owned())))
            }

            fn visit_unit<Error: de::Error>(self) -> Result<Self::Value, Error> {
                Ok(UniqueValue(Value::Null))
            }
        }

        deserializer.deserialize_any(UniqueVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{PluginInstallProvenance, lifecycle::PluginGrant};

    fn fixture() -> PluginInventoryDescriptor {
        let identity = PluginPackageIdentity {
            component_sha256: Some(format!("sha256:{}", "a".repeat(64))),
            provenance: Some(PluginInstallProvenance {
                format_version: 1,
                source: "ghcr.io/example/plugin".into(),
                digest: format!("sha256:{}", "b".repeat(64)),
                signature_policy: "cosign-keyless".into(),
            }),
        };
        let grant = PluginGrant {
            version: "1.0.0".into(),
            capabilities: Vec::new(),
            manifest_digest: "c".repeat(64),
            identity: identity.clone(),
        };
        PluginInventoryDescriptor {
            format_version: 1,
            inventories: BTreeMap::from([(
                "first".into(),
                DescriptorInventory {
                    inventory: PreinstalledInventory {
                        root: "/plugins/first".into(),
                        grants: BTreeMap::from([("plugin".into(), vec![grant])]),
                    },
                    packages: BTreeMap::from([("plugin".into(), identity)]),
                    identity_digest: "d".repeat(64),
                    contract_digest: "e".repeat(64),
                },
            )]),
        }
    }

    #[test]
    fn digest_is_canonical_and_expected_parse_is_exact() {
        let descriptor = fixture();
        let digest = descriptor.digest().unwrap();
        let compact = serde_json::to_vec(&descriptor).unwrap();
        let pretty = serde_json::to_vec_pretty(&descriptor).unwrap();
        let reordered = format!(
            r#"{{"inventories":{},"format_version":1}}"#,
            serde_json::to_string(&descriptor.inventories).unwrap()
        );
        assert_eq!(
            PluginInventoryDescriptor::parse_expected(&compact, &digest)
                .unwrap()
                .digest()
                .unwrap(),
            digest
        );
        assert_eq!(
            PluginInventoryDescriptor::parse(&pretty)
                .unwrap()
                .digest()
                .unwrap(),
            digest
        );
        assert_eq!(
            PluginInventoryDescriptor::parse(reordered.as_bytes())
                .unwrap()
                .digest()
                .unwrap(),
            digest
        );
        assert!(PluginInventoryDescriptor::parse_expected(&compact, &"f".repeat(64)).is_err());
        let references = descriptor.inventories["first"]
            .package_references()
            .unwrap();
        assert_eq!(
            references["plugin"],
            format!("ghcr.io/example/plugin@sha256:{}", "b".repeat(64))
        );
    }

    #[test]
    fn complete_packages_and_provenance_are_required() {
        let original = fixture();
        let mut missing = original.clone();
        missing
            .inventories
            .get_mut("first")
            .unwrap()
            .packages
            .clear();
        assert!(missing.validate().is_err());
        let mut extra = original.clone();
        let identity = extra.inventories["first"].packages["plugin"].clone();
        extra
            .inventories
            .get_mut("first")
            .unwrap()
            .packages
            .insert("extra".into(), identity);
        assert!(extra.validate().is_err());
        let mut legacy = original.clone();
        let entry = legacy.inventories.get_mut("first").unwrap();
        entry.packages.get_mut("plugin").unwrap().provenance = None;
        entry.inventory.grants.get_mut("plugin").unwrap()[0]
            .identity
            .provenance = None;
        assert!(legacy.validate().is_err());
        let mut mismatched = original.clone();
        mismatched
            .inventories
            .get_mut("first")
            .unwrap()
            .packages
            .get_mut("plugin")
            .unwrap()
            .component_sha256 = None;
        assert!(mismatched.validate().is_err());
        let mut wrong_set = original.clone();
        let entry = wrong_set.inventories.get_mut("first").unwrap();
        let identity = entry.packages.remove("plugin").unwrap();
        entry.packages.insert("different".into(), identity);
        assert!(wrong_set.validate().is_err());
    }

    #[test]
    fn history_and_legacy_inventory_maps_are_immutable() {
        let previous = fixture();
        let retained = BTreeMap::from([(
            "first".into(),
            previous.inventories["first"].inventory.clone(),
        )]);
        let mut next = previous.clone();
        let mut empty = next.inventories["first"].clone();
        empty.inventory.root = "/plugins/empty".into();
        empty.inventory.grants.clear();
        empty.packages.clear();
        next.inventories.insert("empty".into(), empty);
        next.validate_extension_of(&previous).unwrap();
        next.validate_retains_inventory(&retained).unwrap();
        let mut removed = next.clone();
        removed.inventories.remove("first");
        assert!(removed.validate_extension_of(&previous).is_err());
        assert!(removed.validate_retains_inventory(&retained).is_err());
        let mut changed = next.clone();
        changed
            .inventories
            .get_mut("first")
            .unwrap()
            .contract_digest = "f".repeat(64);
        assert!(changed.validate_extension_of(&previous).is_err());
        let mut changed_root = next.clone();
        changed_root
            .inventories
            .get_mut("first")
            .unwrap()
            .inventory
            .root = "/plugins/changed".into();
        assert!(changed_root.validate_retains_inventory(&retained).is_err());
        let mut changed_grants = next;
        changed_grants
            .inventories
            .get_mut("first")
            .unwrap()
            .inventory
            .grants
            .get_mut("plugin")
            .unwrap()[0]
            .version = "2.0.0".into();
        assert!(
            changed_grants
                .validate_retains_inventory(&retained)
                .is_err()
        );
        let mut changed_identity = previous.clone();
        let entry = changed_identity.inventories.get_mut("first").unwrap();
        let identity = entry.packages.get_mut("plugin").unwrap();
        identity.component_sha256 = None;
        entry.inventory.grants.get_mut("plugin").unwrap()[0].identity = identity.clone();
        changed_identity.validate().unwrap();
        assert!(changed_identity.validate_extension_of(&previous).is_err());
    }

    #[test]
    fn roots_versions_and_untrusted_json_fail_closed() {
        let original = fixture();
        for root in ["relative", "/", "/plugins/../first"] {
            let mut invalid = original.clone();
            invalid.inventories.get_mut("first").unwrap().inventory.root = root.into();
            assert!(invalid.validate().is_err());
        }
        let mut duplicate_root = original.clone();
        duplicate_root
            .inventories
            .insert("second".into(), original.inventories["first"].clone());
        assert!(duplicate_root.validate().is_err());
        let mut unsupported = original.clone();
        unsupported.format_version = 2;
        assert!(unsupported.validate().is_err());
        assert!(
            PluginInventoryDescriptor::parse(
                br#"{"format_version":1,"format_version":1,"inventories":{}}"#
            )
            .is_err()
        );
        assert!(
            PluginInventoryDescriptor::parse(
                br#"{"format_version":1,"inventories":{},"approved":true}"#
            )
            .is_err()
        );
        let nested = serde_json::to_string(&original).unwrap().replace(
            "\"version\":\"1.0.0\"",
            "\"version\":\"1.0.0\",\"version\":\"1.0.0\"",
        );
        assert!(PluginInventoryDescriptor::parse(nested.as_bytes()).is_err());
        assert!(PluginInventoryDescriptor::parse(&vec![b' '; MAX_DESCRIPTOR_BYTES + 1]).is_err());
        assert!(PluginInventoryDescriptor::parse(b"{} {}").is_err());
        let empty =
            PluginInventoryDescriptor::parse(br#"{"format_version":1,"inventories":{}}"#).unwrap();
        empty.validate().unwrap();
    }

    #[test]
    fn malformed_references_and_receipts_are_rejected() {
        for source in [
            "",
            "https://ghcr.io/example/plugin",
            "user:secret@ghcr.io/example/plugin",
            "ghcr.io/example/plugin?token=secret",
            "ghcr.io/example/plugin\n",
        ] {
            let mut invalid = fixture();
            let entry = invalid.inventories.get_mut("first").unwrap();
            let identity = entry.packages.get_mut("plugin").unwrap();
            identity.provenance.as_mut().unwrap().source = source.into();
            entry.inventory.grants.get_mut("plugin").unwrap()[0].identity = identity.clone();
            assert!(invalid.validate().is_err());
        }
        for digest in ["sha256:short", "blake3:abcd"] {
            let mut invalid = fixture();
            let entry = invalid.inventories.get_mut("first").unwrap();
            let identity = entry.packages.get_mut("plugin").unwrap();
            identity.provenance.as_mut().unwrap().digest = digest.into();
            entry.inventory.grants.get_mut("plugin").unwrap()[0].identity = identity.clone();
            assert!(invalid.validate().is_err());
        }
        let mut invalid = fixture();
        invalid
            .inventories
            .get_mut("first")
            .unwrap()
            .identity_digest = "sha256:wrong".into();
        assert!(invalid.validate().is_err());
        for plugin_id in ["../plugin", "UPPER", "typed-filter"] {
            let mut invalid = fixture();
            let entry = invalid.inventories.get_mut("first").unwrap();
            let identity = entry.packages.remove("plugin").unwrap();
            let grants = entry.inventory.grants.remove("plugin").unwrap();
            entry.packages.insert(plugin_id.into(), identity);
            entry.inventory.grants.insert(plugin_id.into(), grants);
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn package_metadata_changes_affect_digest_and_invalid_provenance_is_rejected() {
        let original = fixture();
        let mut changed = original.clone();
        let entry = changed.inventories.get_mut("first").unwrap();
        let identity = entry.packages.get_mut("plugin").unwrap();
        identity.provenance.as_mut().unwrap().digest = format!("sha256:{}", "f".repeat(64));
        entry.inventory.grants.get_mut("plugin").unwrap()[0].identity = identity.clone();
        assert_ne!(original.digest().unwrap(), changed.digest().unwrap());
        assert!(changed.validate_extension_of(&original).is_err());

        for policy in ["", "unchecked"] {
            let mut invalid = fixture();
            let entry = invalid.inventories.get_mut("first").unwrap();
            let identity = entry.packages.get_mut("plugin").unwrap();
            identity.provenance.as_mut().unwrap().signature_policy = policy.into();
            entry.inventory.grants.get_mut("plugin").unwrap()[0].identity = identity.clone();
            assert!(invalid.validate().is_err());
        }
        let mut invalid = fixture();
        let entry = invalid.inventories.get_mut("first").unwrap();
        let identity = entry.packages.get_mut("plugin").unwrap();
        identity.provenance.as_mut().unwrap().format_version = 2;
        entry.inventory.grants.get_mut("plugin").unwrap()[0].identity = identity.clone();
        assert!(invalid.validate().is_err());
    }
}
