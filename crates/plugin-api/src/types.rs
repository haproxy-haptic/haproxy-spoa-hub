use abi_stable::{
    StableAbi,
    std_types::{RHashMap, RString, RVec},
};

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
}
