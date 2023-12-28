use std::{
    collections::{hash_map::Entry, HashMap},
    fmt::Display,
    io::{Read, Write},
    path::PathBuf,
};

use anyhow::Result;
use etcetera::{choose_base_strategy, BaseStrategy};
use indexmap::IndexMap;
use serde::{de::Visitor, Deserialize, Serialize};
use starknet::core::{
    types::FieldElement,
    utils::{cairo_short_string_to_felt, parse_cairo_short_string},
};
use url::Url;

pub(crate) const DEFAULT_PROFILE_NAME: &str = "default";

#[derive(Debug, Default)]
pub struct Profiles {
    pub profiles: IndexMap<String, Profile>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub networks: IndexMap<String, Network>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(
        serialize_with = "serialize_chain_id",
        deserialize_with = "deserialize_chain_id"
    )]
    pub chain_id: FieldElement,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_integration: bool,
    pub provider: NetworkProvider,
}

#[derive(Debug)]
pub enum NetworkProvider {
    Rpc(Url),
    Free(FreeProviderVendor),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum FreeProviderVendor {
    Blast,
    Nethermind,
}

struct ChainIdVisitor;
struct UrlVisitor;

impl Profiles {
    pub fn load() -> Result<Self> {
        let path = Self::get_profiles_path()?;

        let loaded_profiles = if path.exists() {
            let mut file = std::fs::File::open(path)?;
            let mut buffer = String::new();
            file.read_to_string(&mut buffer)?;

            toml::from_str(&buffer)?
        } else {
            Self::default()
        };

        // Custom profile to be supported in the future
        if loaded_profiles.profiles.len() > 1
            || (loaded_profiles.profiles.len() == 1
                && !loaded_profiles.profiles.contains_key(DEFAULT_PROFILE_NAME))
        {
            anyhow::bail!(
                "invalid profiles: only the `default` profile is supported at the moment"
            );
        }

        if let Some(default_profile) = loaded_profiles.profiles.get(DEFAULT_PROFILE_NAME) {
            // Checks chain ID duplication
            let mut chain_id_last_used_in_network = HashMap::new();
            for (network_id, network) in default_profile.networks.iter() {
                match chain_id_last_used_in_network.entry(network.chain_id) {
                    Entry::Occupied(entry) => anyhow::bail!(
                        "invalid profile `default`: networks {} and {} have the same chain ID",
                        entry.get(),
                        network_id
                    ),
                    Entry::Vacant(entry) => {
                        entry.insert(network_id);
                    }
                }
            }
        }

        Ok(loaded_profiles)
    }

    pub fn save(&self) -> Result<()> {
        let serialized = toml::to_string_pretty(self)?;

        let config_folder = Self::get_config_folder()?;
        if !config_folder.exists() {
            std::fs::create_dir_all(config_folder)?;
        }

        let path = Self::get_profiles_path()?;
        let mut file = std::fs::File::create(path)?;

        file.write_all(serialized.as_bytes())?;

        Ok(())
    }

    fn get_config_folder() -> Result<PathBuf> {
        let strategy = choose_base_strategy()
            .map_err(|_| anyhow::anyhow!("unable to find the config directory"))?;
        let mut path = strategy.config_dir();
        path.push("starkli");
        Ok(path)
    }

    fn get_profiles_path() -> Result<PathBuf> {
        let mut path = Self::get_config_folder()?;
        path.push("profiles.toml");
        Ok(path)
    }
}

impl Serialize for Profiles {
    fn serialize<S>(&self, serializer: S) -> std::prelude::v1::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        #[serde(transparent)]
        struct Transparent<'a>(&'a IndexMap<String, Profile>);

        Transparent(&self.profiles).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Profiles {
    fn deserialize<D>(deserializer: D) -> std::prelude::v1::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, transparent)]
        struct Transparent(IndexMap<String, Profile>);

        Ok(Self {
            profiles: Transparent::deserialize(deserializer)?.0,
        })
    }
}

impl Serialize for NetworkProvider {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Shorthand for `rpc` type as a raw string
        #[derive(Serialize)]
        #[serde(transparent)]
        struct RpcVariant<'a>(&'a str);

        #[derive(Serialize)]
        struct FreeVariant<'a> {
            r#type: &'static str,
            vendor: &'a FreeProviderVendor,
        }

        match self {
            Self::Rpc(value) => RpcVariant(value.as_ref()).serialize(serializer),
            Self::Free(value) => FreeVariant {
                r#type: "free",
                vendor: value,
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for NetworkProvider {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, untagged)]
        enum ShorthandOrTagged {
            Shorthand(#[serde(deserialize_with = "deserialize_url")] Url),
            Tagged(Tagged),
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, tag = "type", rename_all = "snake_case")]
        enum Tagged {
            Rpc(RpcVariant),
            Free(FreeVariant),
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, transparent)]
        struct RpcVariant {
            #[serde(deserialize_with = "deserialize_url")]
            url: Url,
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct FreeVariant {
            vendor: FreeProviderVendor,
        }

        Ok(match ShorthandOrTagged::deserialize(deserializer)? {
            ShorthandOrTagged::Shorthand(value) => Self::Rpc(value),
            ShorthandOrTagged::Tagged(value) => match value {
                Tagged::Rpc(value) => Self::Rpc(value.url),
                Tagged::Free(value) => Self::Free(value.vendor),
            },
        })
    }
}

impl Display for FreeProviderVendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blast => write!(f, "Blast"),
            Self::Nethermind => write!(f, "Nethermind"),
        }
    }
}

impl<'de> Visitor<'de> for ChainIdVisitor {
    type Value = FieldElement;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "string")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        cairo_short_string_to_felt(v).map_err(|_| {
            serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(v),
                &"valid Cairo short string",
            )
        })
    }
}

impl<'de> Visitor<'de> for UrlVisitor {
    type Value = Url;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "string")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Url::parse(v).map_err(|_| {
            serde::de::Error::invalid_value(serde::de::Unexpected::Str(v), &"valid URL")
        })
    }
}

fn serialize_chain_id<S>(value: &FieldElement, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(
        &parse_cairo_short_string(value)
            .map_err(|_| serde::ser::Error::custom("invalid Cairo short string"))?,
    )
}

fn deserialize_chain_id<'de, D>(deserializer: D) -> Result<FieldElement, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserializer.deserialize_str(ChainIdVisitor)
}

fn deserialize_url<'de, D>(deserializer: D) -> Result<Url, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserializer.deserialize_str(UrlVisitor)
}

fn is_false(value: &bool) -> bool {
    value == &false
}
