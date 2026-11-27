use std::sync::Arc;

/// Name of a resource, as given by its folder name. Cheap to clone.
pub type ResourceName = Arc<str>;

/// A language-neutral script value.
///
/// Values cross resource boundaries (events, exports, function references),
/// because every resource has its own isolated script state. Later they also
/// cross the network, which is why strings are raw bytes (Lua strings are not
/// guaranteed to be UTF-8) and why there is no "table identity": a table is
/// copied, never shared.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Value {
    #[default]
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(Box<[u8]>),
    /// A sequence with keys `1..=n` in Lua terms.
    Array(Vec<Value>),
    /// Any other table. Order is unspecified.
    Map(Vec<(Value, Value)>),
    /// `vector2` of a script. Components are `f32`, as in the games.
    Vector2([f32; 2]),
    /// `vector3` of a script.
    Vector3([f32; 3]),
    /// `vector4` of a script.
    Vector4([f32; 4]),
    /// `quat` of a script in `[w, x, y, z]` order.
    Quat([f32; 4]),
    /// A callable owned by another resource (or the same one).
    FuncRef(FuncRef),
}

impl Value {
    /// Returns the string contents if this is valid UTF-8 text.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(bytes) => std::str::from_utf8(bytes).ok(),
            _ => None,
        }
    }

    /// Whether a function reference appears anywhere inside this value.
    pub fn contains_func_ref(&self) -> bool {
        match self {
            Self::FuncRef(_) => true,
            Self::Array(items) => items.iter().any(Self::contains_func_ref),
            Self::Map(pairs) => pairs
                .iter()
                .any(|(key, item)| key.contains_func_ref() || item.contains_func_ref()),
            Self::Nil
            | Self::Bool(_)
            | Self::Int(_)
            | Self::Float(_)
            | Self::String(_)
            | Self::Vector2(_)
            | Self::Vector3(_)
            | Self::Vector4(_)
            | Self::Quat(_) => false,
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Self::String(s.as_bytes().into())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Self::String(s.into_bytes().into_boxed_slice())
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Self::Int(i)
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Self::Float(f)
    }
}

/// Reference to a function living inside a specific runtime instance.
///
/// `instance` changes every time a resource is (re)started, so a reference
/// taken before a restart fails cleanly instead of calling into the wrong code.
///
/// # Lifetime
///
/// A `FuncRef` inside a [`Value`] is a borrowed reference: it stays valid for
/// the synchronous call that carries it, and at least until the owner's next
/// [`tick`](crate::ScriptRuntime::tick). A receiver that keeps the function
/// longer must call [`Host::retain_ref`](crate::Host::retain_ref) once per
/// copy it keeps, and [`Host::release_ref`](crate::Host::release_ref) once per
/// copy when it is done. The owner frees a function when no copy is retained.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FuncRef {
    pub owner: ResourceName,
    pub instance: u64,
    pub id: u32,
}
