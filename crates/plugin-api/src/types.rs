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
                a.len() == b.len()
                    && a.iter()
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

impl PluginContext {
    /// Look up a config value by key name.
    ///
    /// This is a convenience wrapper around `self.config.get(key)` that
    /// accepts `&str` directly.
    #[must_use]
    pub fn get_config(&self, key: &str) -> Option<&ConfigValue> {
        self.config.get(key)
    }
}

impl SpoeMessage {
    /// Look up a message argument by name.
    ///
    /// Uses the underlying `RHashMap` for O(1) lookup without allocation,
    /// since `RString` implements `Borrow<str>`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&SpoeValue> {
        self.args.get(name)
    }

    /// Get an argument as a string slice, or `None` if missing or wrong type.
    #[must_use]
    pub fn get_string(&self, name: &str) -> Option<&str> {
        match self.get(name)? {
            SpoeValue::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Get an argument as an `i64`, coercing from any SPOE integer type.
    ///
    /// Returns `None` if the argument is missing, not an integer type,
    /// or if a `Uint64` value exceeds `i64::MAX`.
    #[must_use]
    pub fn get_int(&self, name: &str) -> Option<i64> {
        match self.get(name)? {
            SpoeValue::Int32(v) => Some(i64::from(*v)),
            SpoeValue::Uint32(v) => Some(i64::from(*v)),
            SpoeValue::Int64(v) => Some(*v),
            SpoeValue::Uint64(v) => i64::try_from(*v).ok(),
            _ => None,
        }
    }

    /// Get an argument as a boolean, or `None` if missing or wrong type.
    #[must_use]
    pub fn get_bool(&self, name: &str) -> Option<bool> {
        match self.get(name)? {
            SpoeValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Get an argument as an IPv4 address (4 octets), or `None` if missing or wrong type.
    #[must_use]
    pub fn get_ipv4(&self, name: &str) -> Option<[u8; 4]> {
        match self.get(name)? {
            SpoeValue::Ipv4(ip) => Some(*ip),
            _ => None,
        }
    }

    /// Get an argument as an IPv6 address (16 octets), or `None` if missing or wrong type.
    #[must_use]
    pub fn get_ipv6(&self, name: &str) -> Option<[u8; 16]> {
        match self.get(name)? {
            SpoeValue::Ipv6(ip) => Some(*ip),
            _ => None,
        }
    }

    /// Get an argument as a byte slice, or `None` if missing or wrong type.
    #[must_use]
    pub fn get_binary(&self, name: &str) -> Option<&[u8]> {
        match self.get(name)? {
            SpoeValue::Binary(b) => Some(b.as_slice()),
            _ => None,
        }
    }
}

impl TxnVariable {
    /// Create a variable with the given scope.
    #[must_use]
    pub fn new(scope: VarScope, name: impl Into<RString>, value: SpoeValue) -> Self {
        Self {
            scope,
            name: name.into(),
            value,
        }
    }

    /// Create a transaction-scoped variable.
    #[must_use]
    pub fn transaction(name: impl Into<RString>, value: SpoeValue) -> Self {
        Self::new(VarScope::Transaction, name, value)
    }

    /// Create a session-scoped variable.
    #[must_use]
    pub fn session(name: impl Into<RString>, value: SpoeValue) -> Self {
        Self::new(VarScope::Session, name, value)
    }
}

impl ProcessingResult {
    /// Create an empty result (no variables to set).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            variables: RVec::new(),
        }
    }

    /// Create a result from a `Vec` of variables.
    #[must_use]
    pub fn from_vars(vars: Vec<TxnVariable>) -> Self {
        Self {
            variables: vars.into(),
        }
    }

    /// Create a result with a single variable.
    #[must_use]
    pub fn single(var: TxnVariable) -> Self {
        Self {
            variables: vec![var].into(),
        }
    }
}

/// Severity of a diagnostic returned by a plugin's `validate()` method.
///
/// ABI-stable; new variants require a plugin-api version bump.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, StableAbi)]
pub enum DiagnosticSeverity {
    /// Surfaced via logs and CRD/admission status, but does not block
    /// `init()` (production mode) or yield `result: "error"` (validator
    /// mode).
    Warning = 0,
    /// Blocks `init()` in production mode and yields `result: "error"`
    /// in validator mode.
    Error = 1,
}

/// One finding produced by a plugin's `validate()` method.
///
/// Position fields (`line`, `column`) are 1-based; `0` indicates
/// "unknown / file-level" — for example, a "this plugin requires field
/// X to be set" diagnostic where there's no specific location to point
/// at, or a parser error that the plugin couldn't extract a position
/// from.
///
/// # `path` ownership
///
/// Plugins MUST leave `path` empty (`RString::new()`) when constructing
/// diagnostics. The hub overwrites `path` post-hoc with the file
/// identity it knows about — in validator-sidecar mode that's the
/// request's `files[].path`; in production mode it's the value from
/// the hub's `--config <path>` flag. Plugins don't have access to
/// file identity (their input is the parsed `params` sub-table from
/// the matching `[[plugins]]` entry, decoupled from the file it came
/// from) and don't need it.
///
/// See `specs/004-validate-mode/contracts/plugin-api-delta.md` for the
/// design rationale.
#[repr(C)]
#[derive(Debug, Clone, StableAbi)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub path: RString,
    pub line: u32,
    pub column: u32,
    pub message: RString,
}

impl Diagnostic {
    /// Construct an error diagnostic at the given line/column. Pass
    /// `0` for either position field when it's not available. The hub
    /// fills `path` later.
    #[must_use]
    pub fn error(line: u32, column: u32, message: impl Into<RString>) -> Self {
        Self {
            severity: DiagnosticSeverity::Error,
            path: RString::new(),
            line,
            column,
            message: message.into(),
        }
    }

    /// Construct a warning diagnostic at the given line/column. Pass
    /// `0` for either position field when it's not available. The hub
    /// fills `path` later.
    #[must_use]
    pub fn warning(line: u32, column: u32, message: impl Into<RString>) -> Self {
        Self {
            severity: DiagnosticSeverity::Warning,
            path: RString::new(),
            line,
            column,
            message: message.into(),
        }
    }
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

    fn make_message(args: Vec<(&str, SpoeValue)>) -> SpoeMessage {
        let mut map = RHashMap::new();
        for (k, v) in args {
            map.insert(RString::from(k), v);
        }
        SpoeMessage {
            name: RString::from("test"),
            args: map,
            stream_id: 1,
            frame_id: 1,
        }
    }

    #[test]
    fn spoe_message_get() {
        let msg = make_message(vec![("key", SpoeValue::String("val".into()))]);
        assert!(msg.get("key").is_some());
        assert!(msg.get("missing").is_none());
    }

    #[test]
    fn spoe_message_get_string() {
        let msg = make_message(vec![
            ("s", SpoeValue::String("hello".into())),
            ("n", SpoeValue::Int32(42)),
        ]);
        assert_eq!(msg.get_string("s"), Some("hello"));
        assert_eq!(msg.get_string("n"), None);
        assert_eq!(msg.get_string("missing"), None);
    }

    #[test]
    fn spoe_message_get_int_coerces() {
        let msg = make_message(vec![
            ("i32", SpoeValue::Int32(-1)),
            ("u32", SpoeValue::Uint32(100)),
            ("i64", SpoeValue::Int64(i64::MIN)),
            ("u64", SpoeValue::Uint64(999)),
            ("u64_overflow", SpoeValue::Uint64(u64::MAX)),
            ("s", SpoeValue::String("nope".into())),
        ]);
        assert_eq!(msg.get_int("i32"), Some(-1));
        assert_eq!(msg.get_int("u32"), Some(100));
        assert_eq!(msg.get_int("i64"), Some(i64::MIN));
        assert_eq!(msg.get_int("u64"), Some(999));
        assert_eq!(msg.get_int("u64_overflow"), None);
        assert_eq!(msg.get_int("s"), None);
    }

    #[test]
    fn spoe_message_get_bool() {
        let msg = make_message(vec![("b", SpoeValue::Bool(true))]);
        assert_eq!(msg.get_bool("b"), Some(true));
        assert_eq!(msg.get_bool("missing"), None);
    }

    #[test]
    fn spoe_message_get_ipv4() {
        let msg = make_message(vec![("ip", SpoeValue::Ipv4([10, 0, 0, 1]))]);
        assert_eq!(msg.get_ipv4("ip"), Some([10, 0, 0, 1]));
        assert_eq!(msg.get_ipv4("missing"), None);
    }

    #[test]
    fn spoe_message_get_ipv6() {
        let addr = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let msg = make_message(vec![("ip", SpoeValue::Ipv6(addr))]);
        assert_eq!(msg.get_ipv6("ip"), Some(addr));
    }

    #[test]
    fn spoe_message_get_binary() {
        let msg = make_message(vec![("b", SpoeValue::Binary(vec![1, 2, 3].into()))]);
        assert_eq!(msg.get_binary("b"), Some([1, 2, 3].as_slice()));
        assert_eq!(msg.get_binary("missing"), None);
    }

    #[test]
    fn txn_variable_constructors() {
        let t = TxnVariable::transaction("x", SpoeValue::Bool(true));
        assert_eq!(t.scope, VarScope::Transaction);
        assert_eq!(t.name.as_str(), "x");

        let s = TxnVariable::session("y", SpoeValue::Int32(1));
        assert_eq!(s.scope, VarScope::Session);
        assert_eq!(s.name.as_str(), "y");

        let n = TxnVariable::new(VarScope::Process, "z", SpoeValue::Null);
        assert_eq!(n.scope, VarScope::Process);
    }

    #[test]
    fn processing_result_constructors() {
        let empty = ProcessingResult::empty();
        assert!(empty.variables.is_empty());

        let single =
            ProcessingResult::single(TxnVariable::transaction("a", SpoeValue::Bool(false)));
        assert_eq!(single.variables.len(), 1);

        let multi = ProcessingResult::from_vars(vec![
            TxnVariable::transaction("a", SpoeValue::Null),
            TxnVariable::transaction("b", SpoeValue::Null),
        ]);
        assert_eq!(multi.variables.len(), 2);
    }

    #[test]
    fn plugin_context_get_config() {
        let mut config = RHashMap::new();
        config.insert(RString::from("timeout"), ConfigValue::Integer(5000));
        let ctx = PluginContext {
            name: RString::from("test"),
            config,
        };
        assert_eq!(
            ctx.get_config("timeout").and_then(ConfigValue::as_integer),
            Some(5000)
        );
        assert!(ctx.get_config("missing").is_none());
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

    #[test]
    fn diagnostic_error_constructor() {
        let d = Diagnostic::error(42, 7, "unknown directive");
        assert_eq!(d.severity, DiagnosticSeverity::Error);
        assert!(d.path.is_empty(), "plugins must leave path empty for hub");
        assert_eq!(d.line, 42);
        assert_eq!(d.column, 7);
        assert_eq!(d.message.as_str(), "unknown directive");
    }

    #[test]
    fn diagnostic_warning_constructor() {
        let d = Diagnostic::warning(0, 0, RString::from("file-level warn"));
        assert_eq!(d.severity, DiagnosticSeverity::Warning);
        assert!(d.path.is_empty());
        assert_eq!(d.line, 0);
        assert_eq!(d.column, 0);
        assert_eq!(d.message.as_str(), "file-level warn");
    }

    #[test]
    fn diagnostic_severity_distinct() {
        assert_ne!(DiagnosticSeverity::Warning, DiagnosticSeverity::Error);
        assert_eq!(DiagnosticSeverity::Warning as u8, 0);
        assert_eq!(DiagnosticSeverity::Error as u8, 1);
    }
}
