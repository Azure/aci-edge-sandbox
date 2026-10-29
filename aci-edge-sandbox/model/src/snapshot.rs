// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Platform snapshots: how a start reports whether it cloned a template, and how a host reports
//! its templates.
//!
//! A setup that enables [`PlatformSnapshots`](crate::PlatformSnapshots) has its host capture two
//! templates when it first opens: microVM snapshots that hold no tenant data, one without a NIC
//! and one with a deny-all NIC on the setup's default guest network. Every start that a template
//! fits then starts as a clone of it, which is faster than a cold boot, and every other start
//! cold boots, as it would without templates. A start's metadata says which, and why
//! ([`StartResult::boot_mode`](crate::StartResult::boot_mode)), and the host's
//! [`HostInfo`](crate::wire::HostInfo) lists the templates.

use std::fmt;
use std::time::SystemTime;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::hex;
use crate::rfc3339;

/// Defines a unit-only enum and the wire spelling of each of its values. Serde writes a value as
/// its spelling, and reads only the spellings: the implementations that serde derives would also
/// read the form `{"spelling": null}`.
macro_rules! spelled_enum {
    (
        $(#[$attribute:meta])*
        pub enum $name:ident {
            $(
                $(#[$variant_attribute:meta])*
                $variant:ident => $spelling:literal,
            )+
        }
    ) => {
        $(#[$attribute])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum $name {
            $(
                $(#[$variant_attribute])*
                $variant,
            )+
        }

        impl $name {
            /// Returns the wire spelling of this value.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $spelling,)+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                match text.as_str() {
                    $($spelling => Ok(Self::$variant),)+
                    _ => Err(serde::de::Error::unknown_variant(&text, &[$($spelling),+])),
                }
            }
        }
    };
}

spelled_enum! {
    /// How a sandbox started: the start metadata's `bootMode`.
    pub enum BootMode {
        /// The sandbox started as a clone of a template.
        Clone => "clone",
        /// The sandbox cold booted: it booted its kernel, as every sandbox does without platform
        /// snapshots.
        Cold => "cold",
    }
}

spelled_enum! {
    /// Why a sandbox cold booted although its host's setup enables platform snapshots: the start
    /// metadata's `coldReason`.
    pub enum ColdReason {
        /// The setup sets `diagnostics.guestDebug`, so the host keeps no template.
        GuestDebug => "guest-debug",
        /// The sandbox has more virtual processors than `platformSnapshots.maxVcpus`.
        VcpusAboveCapacity => "vcpus-above-capacity",
        /// The sandbox's memory is not a whole number of 128 MiB blocks.
        MemoryNotWholeBlocks => "memory-not-whole-blocks",
        /// The sandbox has less memory than the templates boot with, the setup's
        /// `defaults.resources.memoryMib`.
        MemoryBelowTemplate => "memory-below-template",
        /// The sandbox has more memory than `platformSnapshots.maxMemoryMib`.
        MemoryAboveCapacity => "memory-above-capacity",
        /// The sandbox has a NIC on another guest network than the setup's
        /// `defaults.guestNetwork`.
        GuestNetwork => "guest-network",
        /// The sandbox's image is longer than `platformSnapshots.maxImageMib`.
        ImageTooLong => "image-too-long",
        /// No template of the sandbox's class is eligible: none has been captured yet, or the one
        /// of its class expired or was quarantined and is being captured again.
        NoEligibleTemplate => "no-eligible-template",
        /// The library's plan for the clone refused the sandbox for another reason.
        PlanRefused => "plan-refused",
        /// A clone failed because of its template, and the start cold booted instead.
        TemplateFault => "template-fault",
    }
}

spelled_enum! {
    /// Class of a template: whether its clones have a NIC.
    pub enum TemplateClass {
        /// Clones without a NIC: sandboxes whose network policy attaches no network device.
        NoNic => "noNic",
        /// Clones with a NIC on the setup's `defaults.guestNetwork`. The template holds the NIC
        /// under a policy that denies all traffic, and each clone applies its own.
        Nic => "nic",
    }
}

/// Identity of a template: a digest of what it was captured with, such as the runtime files, the
/// hypervisor, and the capacities, written as 64 lowercase hexadecimal digits. When any of them
/// changes, the template has a new key, and the host captures it anew.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TemplateKey([u8; 32]);

impl TemplateKey {
    /// Returns the key with these bytes.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the key's bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for TemplateKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(&self.0))
    }
}

impl fmt::Debug for TemplateKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "TemplateKey({self})")
    }
}

impl Serialize for TemplateKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for TemplateKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        hex::decode_sha256(&text).map(Self).ok_or_else(|| {
            serde::de::Error::custom("a template key must be 64 lowercase hexadecimal digits")
        })
    }
}

/// The template that a clone started from: the start metadata's `template`, an object of these
/// fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TemplateGeneration {
    /// The template's class.
    pub class: TemplateClass,
    /// The template's key.
    pub key: TemplateKey,
    /// The capture of the template that the clone restored. Each capture of a key has a new
    /// generation.
    pub generation: u64,
}

impl TemplateGeneration {
    /// Names a generation of a template.
    pub fn new(class: TemplateClass, key: TemplateKey, generation: u64) -> Self {
        Self {
            class,
            key,
            generation,
        }
    }
}

impl<'de> Deserialize<'de> for TemplateGeneration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Fields {
            class: TemplateClass,
            key: TemplateKey,
            generation: u64,
        }

        // Reads only an object: the implementation that serde derives would also read the
        // fields from a sequence, in order.
        struct Object;

        impl<'de> serde::de::Visitor<'de> for Object {
            type Value = TemplateGeneration;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object of class, key, and generation")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Self::Value, A::Error> {
                let Fields {
                    class,
                    key,
                    generation,
                } = Fields::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(TemplateGeneration::new(class, key, generation))
            }
        }

        deserializer.deserialize_map(Object)
    }
}

/// Platform snapshots of an open host, which
/// [`HostInfo::platform_snapshots`](crate::wire::HostInfo::platform_snapshots) reports when its
/// setup enables them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct PlatformSnapshotsInfo {
    /// Why the host keeps no template although its setup enables platform snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<DisabledReason>,
    /// The host's templates, one of each class that it keeps.
    #[serde(default)]
    pub templates: Vec<TemplateInfo>,
}

impl PlatformSnapshotsInfo {
    /// Describes a host that keeps these templates.
    pub fn new(templates: Vec<TemplateInfo>) -> Self {
        Self {
            disabled_reason: None,
            templates,
        }
    }

    /// Describes a host that keeps no template, for `reason`.
    pub fn disabled(reason: DisabledReason) -> Self {
        Self {
            disabled_reason: Some(reason),
            templates: Vec::new(),
        }
    }
}

spelled_enum! {
    /// Why a host keeps no template although its setup enables platform snapshots.
    pub enum DisabledReason {
        /// The setup sets `diagnostics.guestDebug`, so every start cold boots, with
        /// [`ColdReason::GuestDebug`].
        GuestDebug => "guest-debug",
    }
}

/// A template of an open host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct TemplateInfo {
    /// The template's class.
    pub class: TemplateClass,
    /// Whether starts can clone the template.
    pub state: TemplateState,
    /// The template's key, when a generation of it exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<TemplateKey>,
    /// The template's newest generation, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// When that generation was captured, written as RFC 3339 in UTC, such as
    /// `2026-10-29T07:00:00Z`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "rfc3339::option"
    )]
    pub captured_at: Option<SystemTime>,
    /// The last failed capture or refresh of the template, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl TemplateInfo {
    /// Describes a template of `class` in `state`, without a generation.
    pub fn new(class: TemplateClass, state: TemplateState) -> Self {
        Self {
            class,
            state,
            key: None,
            generation: None,
            captured_at: None,
            last_error: None,
        }
    }

    /// Sets the template's key, its newest generation, and when that generation was captured.
    #[must_use]
    pub fn with_generation(
        mut self,
        key: TemplateKey,
        generation: u64,
        captured_at: SystemTime,
    ) -> Self {
        self.key = Some(key);
        self.generation = Some(generation);
        self.captured_at = Some(captured_at);
        self
    }

    /// Sets the last failed capture or refresh of the template.
    #[must_use]
    pub fn with_last_error(mut self, error: impl Into<String>) -> Self {
        self.last_error = Some(error.into());
        self
    }
}

spelled_enum! {
    /// Whether starts can clone a template.
    pub enum TemplateState {
        /// A generation is eligible, and every start that it fits clones it.
        Ready => "ready",
        /// No generation is eligible, so the starts of its class cold boot, with
        /// [`ColdReason::NoEligibleTemplate`], until the host captures the template again.
        Unavailable => "unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_have_their_wire_spelling() {
        fn spelled<T: Serialize + for<'de> Deserialize<'de> + PartialEq + fmt::Debug>(
            value: T,
            text: &str,
            display: String,
        ) {
            let json = serde_json::to_value(&value).unwrap();
            assert_eq!(json, serde_json::Value::from(text), "{value:?}");
            assert_eq!(serde_json::from_value::<T>(json).unwrap(), value);
            assert_eq!(display, text);
            // Only the spelling itself, not the form that serde's derived implementations also
            // read, `{"spelling": null}`.
            let tagged = format!(r#"{{"{text}":null}}"#);
            assert!(serde_json::from_str::<T>(&tagged).is_err(), "{tagged}");
            let tagged = serde_json::from_str::<serde_json::Value>(&tagged).unwrap();
            assert!(serde_json::from_value::<T>(tagged).is_err(), "{value:?}");
        }
        for (mode, text) in [(BootMode::Clone, "clone"), (BootMode::Cold, "cold")] {
            assert_eq!(mode.as_str(), text);
            spelled(mode, text, mode.to_string());
        }
        for (reason, text) in [
            (ColdReason::GuestDebug, "guest-debug"),
            (ColdReason::VcpusAboveCapacity, "vcpus-above-capacity"),
            (ColdReason::MemoryNotWholeBlocks, "memory-not-whole-blocks"),
            (ColdReason::MemoryBelowTemplate, "memory-below-template"),
            (ColdReason::MemoryAboveCapacity, "memory-above-capacity"),
            (ColdReason::GuestNetwork, "guest-network"),
            (ColdReason::ImageTooLong, "image-too-long"),
            (ColdReason::NoEligibleTemplate, "no-eligible-template"),
            (ColdReason::PlanRefused, "plan-refused"),
            (ColdReason::TemplateFault, "template-fault"),
        ] {
            assert_eq!(reason.as_str(), text);
            spelled(reason, text, reason.to_string());
        }
        for (class, text) in [(TemplateClass::NoNic, "noNic"), (TemplateClass::Nic, "nic")] {
            assert_eq!(class.as_str(), text);
            spelled(class, text, class.to_string());
        }
        for (state, text) in [
            (TemplateState::Ready, "ready"),
            (TemplateState::Unavailable, "unavailable"),
        ] {
            assert_eq!(state.as_str(), text);
            spelled(state, text, state.to_string());
        }
        let reason = DisabledReason::GuestDebug;
        assert_eq!(reason.as_str(), ColdReason::GuestDebug.as_str());
        spelled(reason, "guest-debug", reason.to_string());
        for text in [
            r#""Clone""#,
            r#""guest_debug""#,
            r#""noNIC""#,
            r#""no-nic""#,
        ] {
            assert!(serde_json::from_str::<BootMode>(text).is_err(), "{text}");
            assert!(serde_json::from_str::<ColdReason>(text).is_err(), "{text}");
            assert!(
                serde_json::from_str::<TemplateClass>(text).is_err(),
                "{text}"
            );
        }
    }

    #[test]
    fn template_keys_are_64_lowercase_hexadecimal_digits() {
        let key = TemplateKey::from_bytes([0xab; 32]);
        assert_eq!(key.as_bytes(), &[0xab; 32]);
        assert_eq!(key.to_string(), "ab".repeat(32));
        assert_eq!(
            format!("{key:?}"),
            format!("TemplateKey({})", "ab".repeat(32))
        );
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(json, format!(r#""{}""#, "ab".repeat(32)));
        assert_eq!(serde_json::from_str::<TemplateKey>(&json).unwrap(), key);
        for text in [
            String::new(),
            "AB".repeat(32),
            "ab".repeat(31),
            "ab".repeat(33),
            format!("sha256:{}", "ab".repeat(32)),
        ] {
            let json = serde_json::Value::from(text.as_str());
            assert!(
                serde_json::from_value::<TemplateKey>(json).is_err(),
                "{text}"
            );
        }
    }

    #[test]
    fn template_generations_are_objects_of_their_three_fields() {
        let key = "ab".repeat(32);
        let object = format!(r#"{{"class":"noNic","key":"{key}","generation":7}}"#);
        let generation =
            TemplateGeneration::new(TemplateClass::NoNic, TemplateKey::from_bytes([0xab; 32]), 7);
        assert_eq!(serde_json::to_string(&generation).unwrap(), object);
        assert_eq!(
            serde_json::from_str::<TemplateGeneration>(&object).unwrap(),
            generation
        );
        for json in [
            // The fields in order, which serde's derived implementation would read.
            format!(r#"["noNic","{key}",7]"#),
            format!(r#"{{"class":"noNic","key":"{key}"}}"#),
            format!(r#"{{"class":"noNic","key":"{key}","generation":7,"state":"ready"}}"#),
            "null".to_owned(),
        ] {
            assert!(
                serde_json::from_str::<TemplateGeneration>(&json).is_err(),
                "{json}"
            );
            let value = serde_json::from_str::<serde_json::Value>(&json).unwrap();
            assert!(
                serde_json::from_value::<TemplateGeneration>(value).is_err(),
                "{json}"
            );
        }
    }
}
