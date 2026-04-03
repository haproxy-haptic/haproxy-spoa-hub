use abi_stable::{
    StableAbi,
    std_types::{RHashMap, RString, RVec, Tuple2},
};

/// A typed configuration value representing a TOML value tree.
///
/// The hub converts each plugin's `[plugins.params]` TOML table into
/// a tree of `ConfigValue` nodes, preserving native TOML types across
/// the FFI boundary. Plugins receive this in [`PluginContext::config`].
///
/// # Examples
///
/// ```rust,ignore
/// use haproxy_spoa_hub_plugin_api::ConfigValue;
///
/// let s = ConfigValue::String("hello".into());
/// let n = ConfigValue::Integer(42);
/// let b = ConfigValue::Bool(true);
/// ```
#[repr(u8)]
#[derive(Debug, Clone, StableAbi)]
pub enum ConfigValue {
    /// UTF-8 string value. Also used for TOML datetime values (stringified).
    String(RString),
    /// Signed 64-bit integer.
    Integer(i64),
    /// 64-bit floating point.
    Float(f64),
    /// Boolean value.
    Bool(bool),
    /// Ordered array of values.
    Array(RVec<ConfigValue>),
    /// Key-value table (nested map).
    Table(RHashMap<RString, ConfigValue>),
    /// Reserved for future value types. Plugins should handle this
    /// gracefully (e.g., skip unknown values or treat as null).
    /// The `u8` preserves the original discriminant for roundtripping.
    #[doc(hidden)]
    __Other(u8, RVec<u8>),
}

impl ConfigValue {
    /// Returns the contained string slice, or `None` if not a `String`.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Returns the contained integer, or `None` if not an `Integer`.
    #[must_use]
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// Returns the contained float, or `None` if not a `Float`.
    #[must_use]
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Self::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// Returns the contained boolean, or `None` if not a `Bool`.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Returns a reference to the contained array, or `None` if not an `Array`.
    #[must_use]
    pub fn as_array(&self) -> Option<&RVec<ConfigValue>> {
        match self {
            Self::Array(a) => Some(a),
            _ => None,
        }
    }

    /// Returns a reference to the contained table, or `None` if not a `Table`.
    #[must_use]
    pub fn as_table(&self) -> Option<&RHashMap<RString, ConfigValue>> {
        match self {
            Self::Table(t) => Some(t),
            _ => None,
        }
    }
}

impl PartialEq for ConfigValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Integer(a), Self::Integer(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a == b,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Array(a), Self::Array(b)) => a == b,
            (Self::Table(a), Self::Table(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                a.iter()
                    .all(|Tuple2(k, v)| b.get(k).is_some_and(|bv| v == bv))
            }
            (Self::__Other(t1, d1), Self::__Other(t2, d2)) => t1 == t2 && d1 == d2,
            _ => false,
        }
    }
}

/// Configuration and metadata passed to a plugin during initialization.
///
/// The hub constructs this from the plugin's `[[plugins]]` config entry
/// and passes it to `init()`. Use this to receive file paths, thresholds,
/// credentials, and other per-plugin settings.
///
/// # Examples
///
/// ```rust,ignore
/// fn init(&mut self, context: &PluginContext)
///     -> Result<(), Box<dyn std::error::Error + Send + Sync>>
/// {
///     let db_path = context.config.get(&"db_path".into())
///         .and_then(|v| v.as_str())
///         .ok_or("missing db_path config")?;
///     self.db = open_database(db_path)?;
///     Ok(())
/// }
/// ```
#[repr(C)]
#[derive(Debug, Clone, StableAbi)]
pub struct PluginContext {
    /// Plugin name as declared in the hub configuration.
    pub name: RString,
    /// Typed parameters from the `[plugins.params]` TOML table.
    /// Supports nested tables, arrays, and all native TOML types.
    pub config: RHashMap<RString, ConfigValue>,
}

/// A typed value from an SPOE message argument or plugin output.
///
/// Maps directly to the SPOP typed-data encoding. The hub converts
/// between `spop::types::TypedData` and this FFI-safe representation
/// so plugins never need to handle wire-format parsing.
///
/// # Examples
///
/// ```rust,ignore
/// use haproxy_spoa_hub_plugin_api::SpoeValue;
///
/// let s = SpoeValue::String("hello".into());
/// let n = SpoeValue::Uint32(42);
/// let ip = SpoeValue::Ipv4([10, 0, 0, 1]);
/// ```
#[repr(u8)]
#[derive(Debug, Clone, StableAbi)]
pub enum SpoeValue {
    /// No value.
    Null,
    /// Boolean value.
    Bool(bool),
    /// Signed 32-bit integer.
    Int32(i32),
    /// Unsigned 32-bit integer.
    Uint32(u32),
    /// Signed 64-bit integer.
    Int64(i64),
    /// Unsigned 64-bit integer.
    Uint64(u64),
    /// IPv4 address as 4 octets in network byte order.
    Ipv4([u8; 4]),
    /// IPv6 address as 16 octets in network byte order.
    Ipv6([u8; 16]),
    /// UTF-8 string. Uses `RString` for FFI safety.
    String(RString),
    /// Raw byte buffer. Uses `RVec<u8>` for FFI safety.
    Binary(RVec<u8>),
    /// Reserved for future SPOP typed-data variants. The `u8` preserves
    /// the original type tag for roundtripping.
    #[doc(hidden)]
    __Other(u8, RVec<u8>),
}

/// Pre-parsed SPOE message passed to plugins by the hub.
///
/// The hub parses each SPOP NOTIFY frame and converts message
/// arguments into typed `SpoeValue` entries so plugins receive
/// structured data without wire-format parsing.
///
/// When a plugin depends on another, the upstream plugin's
/// namespace-prefixed output variables are merged into `args`
/// alongside the original SPOE message arguments.
#[repr(C)]
#[derive(Debug, Clone, StableAbi)]
pub struct SpoeMessage {
    /// SPOE message name as configured in `HAProxy` (e.g., `"check-request"`).
    pub name: RString,
    /// Typed message arguments, keyed by argument name.
    /// Includes both original SPOE args and upstream dependency results.
    pub args: RHashMap<RString, SpoeValue>,
    /// `HAProxy` stream identifier for this message.
    pub stream_id: u64,
    /// `HAProxy` frame identifier for this message.
    pub frame_id: u64,
}

/// The result returned by a plugin after processing a message.
///
/// Contains zero or more transaction variables that the hub will
/// set in `HAProxy` via the ACK response. Variable names are
/// automatically prefixed with the plugin name by the hub.
///
/// # Examples
///
/// ```rust,ignore
/// use haproxy_spoa_hub_plugin_api::{ProcessingResult, TxnVariable, VarScope, SpoeValue};
///
/// let result = ProcessingResult {
///     variables: vec![
///         TxnVariable {
///             scope: VarScope::Session,
///             name: "country_code".into(),
///             value: SpoeValue::String("DE".into()),
///         },
///     ].into(),
/// };
/// ```
#[repr(C)]
#[derive(Debug, Clone, StableAbi)]
pub struct ProcessingResult {
    /// Transaction variables to set in `HAProxy`.
    pub variables: RVec<TxnVariable>,
}

/// A single transaction variable set by a plugin.
///
/// The `name` field should be **unprefixed** — the hub automatically
/// prefixes it with the plugin name. For example, a plugin named
/// `"geoip"` setting `name: "country_code"` results in the `HAProxy`
/// variable `geoip.country_code`.
#[repr(C)]
#[derive(Debug, Clone, StableAbi)]
pub struct TxnVariable {
    /// Variable scope in `HAProxy`, controlling lifetime and visibility.
    pub scope: VarScope,
    /// Variable name without plugin namespace prefix.
    pub name: RString,
    /// Variable value.
    pub value: SpoeValue,
}

/// Variable scope in `HAProxy`, controlling lifetime and visibility.
///
/// Determines how long the variable persists and where it is
/// accessible within `HAProxy`'s processing pipeline.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, StableAbi)]
pub enum VarScope {
    /// Persists for the entire `HAProxy` process lifetime.
    Process = 0,
    /// Persists for the duration of the `HAProxy` session.
    Session = 1,
    /// Persists for the current transaction only.
    Transaction = 2,
    /// Persists for the current request only.
    Request = 3,
    /// Persists for the current response only.
    Response = 4,
    /// Reserved for future scope types. The `u8` preserves the
    /// original scope value for roundtripping.
    #[doc(hidden)]
    __Other(u8),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_value_as_str() {
        let v = ConfigValue::String("hello".into());
        assert_eq!(v.as_str(), Some("hello"));
        assert_eq!(ConfigValue::Integer(1).as_str(), None);
    }

    #[test]
    fn config_value_as_integer() {
        let v = ConfigValue::Integer(42);
        assert_eq!(v.as_integer(), Some(42));
        assert_eq!(ConfigValue::String("x".into()).as_integer(), None);
    }

    #[test]
    fn config_value_as_float() {
        let v = ConfigValue::Float(2.72);
        assert_eq!(v.as_float(), Some(2.72));
        assert_eq!(ConfigValue::Integer(1).as_float(), None);
    }

    #[test]
    fn config_value_as_bool() {
        let v = ConfigValue::Bool(true);
        assert_eq!(v.as_bool(), Some(true));
        assert_eq!(ConfigValue::String("true".into()).as_bool(), None);
    }

    #[test]
    fn config_value_as_array() {
        let arr: RVec<ConfigValue> = vec![ConfigValue::Integer(1), ConfigValue::Integer(2)].into();
        let v = ConfigValue::Array(arr.clone());
        assert_eq!(v.as_array(), Some(&arr));
        assert_eq!(ConfigValue::Integer(1).as_array(), None);
    }

    #[test]
    fn config_value_as_table() {
        let mut map = RHashMap::new();
        map.insert(RString::from("key"), ConfigValue::Bool(true));
        let v = ConfigValue::Table(map.clone());
        assert!(v.as_table().is_some());
        assert_eq!(ConfigValue::Integer(1).as_table(), None);
    }

    #[test]
    fn config_value_partial_eq() {
        assert_eq!(
            ConfigValue::String("a".into()),
            ConfigValue::String("a".into())
        );
        assert_ne!(
            ConfigValue::String("a".into()),
            ConfigValue::String("b".into())
        );
        assert_ne!(ConfigValue::String("1".into()), ConfigValue::Integer(1));
        assert_eq!(ConfigValue::Integer(42), ConfigValue::Integer(42));
        assert_eq!(ConfigValue::Float(1.0), ConfigValue::Float(1.0));
        assert_eq!(ConfigValue::Bool(true), ConfigValue::Bool(true));

        let arr1: RVec<ConfigValue> = vec![ConfigValue::Integer(1)].into();
        let arr2: RVec<ConfigValue> = vec![ConfigValue::Integer(1)].into();
        assert_eq!(ConfigValue::Array(arr1), ConfigValue::Array(arr2));

        let mut t1 = RHashMap::new();
        t1.insert(RString::from("k"), ConfigValue::Integer(1));
        let mut t2 = RHashMap::new();
        t2.insert(RString::from("k"), ConfigValue::Integer(1));
        assert_eq!(ConfigValue::Table(t1), ConfigValue::Table(t2));
    }
}
