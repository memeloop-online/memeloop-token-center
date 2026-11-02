mod factory;
#[cfg(test)]
pub(crate) mod tests;

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};

pub use factory::{ClientOptions, HttpClient, HttpClientFactory, HttpClientSource};
pub(crate) use factory::{HttpSnapshot, scoped};

pub(crate) const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpPurpose {
    General,
    Codex,
    Oauth,
    Plugin,
    PluginRelease,
    Probe,
}

impl HttpPurpose {
    pub const ALL: [Self; 6] = [
        Self::General,
        Self::Codex,
        Self::Oauth,
        Self::Plugin,
        Self::PluginRelease,
        Self::Probe,
    ];
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Override<T> {
    #[default]
    Inherit,
    Value(T),
}

impl<T> Override<T> {
    fn is_inherit(&self) -> bool {
        matches!(self, Self::Inherit)
    }
}

impl<T: Serialize> Serialize for Override<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Inherit => serializer.serialize_none(),
            Self::Value(value) => value.serialize(serializer),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Override<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self::Value)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpOverrides {
    #[serde(skip_serializing_if = "Override::is_inherit")]
    pub connect_timeout_ms: Override<Option<u64>>,
    #[serde(skip_serializing_if = "Override::is_inherit")]
    pub read_timeout_ms: Override<Option<u64>>,
    #[serde(skip_serializing_if = "Override::is_inherit")]
    pub request_timeout_ms: Override<Option<u64>>,
    #[serde(skip_serializing_if = "Override::is_inherit")]
    pub pool_idle_timeout_ms: Override<Option<u64>>,
    #[serde(skip_serializing_if = "Override::is_inherit")]
    pub pool_max_idle_per_host: Override<usize>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpSettings {
    pub defaults: HttpOverrides,
    pub purposes: BTreeMap<HttpPurpose, HttpOverrides>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpSettingsDocument {
    pub revision: u64,
    pub settings: HttpSettings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSource {
    PurposeOverride,
    GlobalDefault,
    SourceDefault,
}

#[derive(Clone, Debug, Serialize)]
pub struct EffectiveValue<T> {
    pub value: T,
    pub source: ValueSource,
    pub unit: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct EffectiveHttpSettings {
    pub connect_timeout_ms: EffectiveValue<Option<u64>>,
    pub read_timeout_ms: EffectiveValue<Option<u64>>,
    pub request_timeout_ms: EffectiveValue<Option<u64>>,
    pub pool_idle_timeout_ms: EffectiveValue<Option<u64>>,
    pub pool_max_idle_per_host: EffectiveValue<usize>,
}

#[derive(Serialize)]
pub struct HttpSettingsView {
    pub revision: u64,
    pub settings: HttpSettings,
    pub effective: BTreeMap<HttpPurpose, EffectiveHttpSettings>,
}

fn resolve<T: Clone>(
    purpose: &Override<T>,
    global: &Override<T>,
    default: T,
    unit: &'static str,
) -> EffectiveValue<T> {
    let (value, source) = match (purpose, global) {
        (Override::Value(value), _) => (value.clone(), ValueSource::PurposeOverride),
        (_, Override::Value(value)) => (value.clone(), ValueSource::GlobalDefault),
        _ => (default, ValueSource::SourceDefault),
    };
    EffectiveValue {
        value,
        source,
        unit,
    }
}

impl HttpSettings {
    pub fn validate(&self) -> Result<(), &'static str> {
        for settings in std::iter::once(&self.defaults).chain(self.purposes.values()) {
            for timeout in [
                &settings.connect_timeout_ms,
                &settings.read_timeout_ms,
                &settings.request_timeout_ms,
                &settings.pool_idle_timeout_ms,
            ] {
                if let Override::Value(Some(value)) = timeout
                    && (*value == 0 || *value > MAX_SAFE_INTEGER)
                {
                    return Err(
                        "HTTP durations must be positive safe integers in milliseconds or null",
                    );
                }
            }
            if let Override::Value(value) = settings.pool_max_idle_per_host
                && value as u128 > u128::from(MAX_SAFE_INTEGER)
            {
                return Err("HTTP pool size must be a nonnegative safe integer");
            }
        }
        Ok(())
    }

    pub fn effective(&self, purpose: HttpPurpose) -> EffectiveHttpSettings {
        let empty = HttpOverrides::default();
        let overrides = self.purposes.get(&purpose).unwrap_or(&empty);
        EffectiveHttpSettings {
            connect_timeout_ms: resolve(
                &overrides.connect_timeout_ms,
                &self.defaults.connect_timeout_ms,
                None,
                "milliseconds",
            ),
            read_timeout_ms: resolve(
                &overrides.read_timeout_ms,
                &self.defaults.read_timeout_ms,
                None,
                "milliseconds",
            ),
            request_timeout_ms: resolve(
                &overrides.request_timeout_ms,
                &self.defaults.request_timeout_ms,
                None,
                "milliseconds",
            ),
            pool_idle_timeout_ms: resolve(
                &overrides.pool_idle_timeout_ms,
                &self.defaults.pool_idle_timeout_ms,
                None,
                "milliseconds",
            ),
            pool_max_idle_per_host: resolve(
                &overrides.pool_max_idle_per_host,
                &self.defaults.pool_max_idle_per_host,
                8,
                "connections",
            ),
        }
    }
}

impl HttpSettingsDocument {
    pub fn view(&self) -> HttpSettingsView {
        HttpSettingsView {
            revision: self.revision,
            settings: self.settings.clone(),
            effective: HttpPurpose::ALL
                .into_iter()
                .map(|purpose| (purpose, self.settings.effective(purpose)))
                .collect(),
        }
    }
}
