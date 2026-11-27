//! The `vector2`, `vector3` and `vector4` types of scripts.
//!
//! A vector is a userdata. FiveM makes vectors values, but here they are
//! shared: `local b = a; b.x = 1` also changes `a`. Arithmetic always returns
//! a new vector, and only assignment to a field changes one in place.

use std::fmt;

use mlua::{
    Function, Lua, MetaMethod, MultiValue, Table, UserData, UserDataMethods, Value as LuaValue,
};
use sixm_core::Value;

/// Largest number of components, which is `vector4`.
const MAX_LEN: usize = 4;
/// Field names of the components, in order.
const NAMES: [&str; MAX_LEN] = ["x", "y", "z", "w"];
/// Letters that name the components, in the same order as [`NAMES`].
const LETTERS: &[u8; MAX_LEN] = b"xyzw";

/// A `vector2`, `vector3` or `vector4` as scripts see it.
///
/// `len` is 2, 3 or 4, and components past `len` are always zero.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Vector {
    len: u8,
    c: [f32; MAX_LEN],
}

impl Vector {
    /// A vector with the given components. The compile-time check keeps `N` to 2, 3 or 4.
    pub(crate) fn new<const N: usize>(components: [f32; N]) -> Self {
        const { assert!(matches!(N, 2..=4), "a vector has 2 to 4 components") }
        let mut c = [0.0; MAX_LEN];
        for (slot, value) in c.iter_mut().zip(components) {
            *slot = value;
        }
        Self {
            len: u8::try_from(N).unwrap_or(0),
            c,
        }
    }

    /// Builds a vector of `len` components, each computed from its index.
    fn from_fn(len: u8, mut component: impl FnMut(usize) -> f32) -> Self {
        let mut c = [0.0; MAX_LEN];
        for (index, slot) in c.iter_mut().take(usize::from(len)).enumerate() {
            *slot = component(index);
        }
        Self { len, c }
    }

    /// The components, from `x` onward.
    fn components(&self) -> &[f32] {
        &self.c[..usize::from(self.len)]
    }

    /// The name scripts see from `type` and `tostring`.
    fn name(&self) -> &'static str {
        match self.len {
            2 => "vector2",
            3 => "vector3",
            _ => "vector4",
        }
    }

    /// The component named by `letter` (`x`, `y`, `z` or `w`), if this vector has it.
    fn field(&self, letter: u8) -> Option<f32> {
        let index = LETTERS.iter().position(|known| *known == letter)?;
        self.at(index)
    }

    /// The component at a zero-based `index`, if this vector has it.
    fn at(&self, index: usize) -> Option<f32> {
        if index < usize::from(self.len) {
            self.c.get(index).copied()
        } else {
            None
        }
    }

    /// The fields that `json.encode` writes: each component name with its value.
    pub(crate) fn fields(&self) -> impl Iterator<Item = (&'static str, f32)> + '_ {
        NAMES.iter().copied().zip(self.components().iter().copied())
    }

    /// The script value this vector converts to.
    pub(crate) fn to_value(self) -> Value {
        match self.len {
            2 => Value::Vector2([self.c[0], self.c[1]]),
            3 => Value::Vector3([self.c[0], self.c[1], self.c[2]]),
            _ => Value::Vector4(self.c),
        }
    }

    /// Euclidean length, which is what `#vector` returns.
    fn length(&self) -> f64 {
        self.components()
            .iter()
            .map(|component| f64::from(*component) * f64::from(*component))
            .sum::<f64>()
            .sqrt()
    }

    /// The same vector with every component negated.
    fn negated(&self) -> Self {
        Self::from_fn(self.len, |index| -self.c[index])
    }

    /// The letters `letters` name, in order, as a vector of that length. `None`
    /// when a letter names a component this vector does not have.
    fn swizzle(&self, letters: &[u8]) -> Option<Self> {
        let len = u8::try_from(letters.len()).ok()?;
        let mut c = [0.0; MAX_LEN];
        for (slot, letter) in c.iter_mut().zip(letters) {
            *slot = self.field(*letter)?;
        }
        Some(Self { len, c })
    }
}

impl fmt::Display for Vector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())?;
        f.write_str("(")?;
        for (index, component) in self.components().iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{component:?}")?;
        }
        f.write_str(")")
    }
}

/// Arithmetic on two vectors or a vector and a number.
#[derive(Clone, Copy)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
}

impl Op {
    fn apply(self, left: f32, right: f32) -> f32 {
        match self {
            Self::Add => left + right,
            Self::Sub => left - right,
            Self::Mul => left * right,
            Self::Div => left / right,
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Sub => "subtract",
            Self::Mul => "multiply",
            Self::Div => "divide",
        }
    }
}

impl UserData for Vector {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_function(MetaMethod::Add, |_, (left, right): (LuaValue, LuaValue)| {
            arithmetic(Op::Add, &left, &right)
        });
        methods.add_meta_function(MetaMethod::Sub, |_, (left, right): (LuaValue, LuaValue)| {
            arithmetic(Op::Sub, &left, &right)
        });
        methods.add_meta_function(MetaMethod::Mul, |_, (left, right): (LuaValue, LuaValue)| {
            arithmetic(Op::Mul, &left, &right)
        });
        methods.add_meta_function(MetaMethod::Div, |_, (left, right): (LuaValue, LuaValue)| {
            arithmetic(Op::Div, &left, &right)
        });
        methods.add_meta_method(MetaMethod::Unm, |_, this, (): ()| Ok(this.negated()));
        methods.add_meta_method(MetaMethod::Len, |_, this, (): ()| Ok(this.length()));
        methods.add_meta_method(MetaMethod::Eq, |_, this, other: LuaValue| {
            Ok(read(&other) == Some(*this))
        });
        methods.add_meta_method(MetaMethod::ToString, |_, this, (): ()| Ok(this.to_string()));
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: LuaValue| {
            index(lua, this, &key)
        });
        methods.add_meta_method_mut(
            MetaMethod::NewIndex,
            |_, this, (key, value): (LuaValue, LuaValue)| assign(this, &key, &value),
        );
    }
}

/// Installs the `vector2`, `vector3` and `vector4` constructors with their
/// `vec` aliases, and replaces `type` so that it names vectors.
///
/// `original_type` is the sandbox's own `type`, which every other value uses.
pub(crate) fn install(lua: &Lua, globals: &Table, original_type: Function) -> mlua::Result<()> {
    let constructors: [(&'static str, u8); 6] = [
        ("vector2", 2),
        ("vec2", 2),
        ("vector3", 3),
        ("vec3", 3),
        ("vector4", 4),
        ("vec4", 4),
    ];
    for (name, len) in constructors {
        let constructor =
            lua.create_function(move |_, args: MultiValue| construct(name, len, args))?;
        globals.raw_set(name, constructor)?;
    }

    // `type` is called constantly, so the usual types are answered here instead
    // of calling back into the original. It only handles what this mapping
    // does not know, and calls without an argument, whose error it raises.
    let type_of = lua.create_function(move |lua, args: MultiValue| {
        let name = match args.front() {
            Some(LuaValue::Nil) => "nil",
            Some(LuaValue::Boolean(_)) => "boolean",
            Some(LuaValue::Integer(_) | LuaValue::Number(_)) => "number",
            Some(LuaValue::String(_)) => "string",
            Some(LuaValue::Table(_)) => "table",
            Some(LuaValue::Function(_)) => "function",
            Some(LuaValue::Thread(_)) => "thread",
            Some(LuaValue::UserData(data)) => match data.borrow::<Vector>() {
                Ok(vector) => vector.name(),
                Err(_) => "userdata",
            },
            Some(LuaValue::LightUserData(_) | LuaValue::Error(_)) => "userdata",
            _ => return original_type.call(args),
        };
        Ok(MultiValue::from_vec(vec![LuaValue::String(
            lua.create_string(name)?,
        )]))
    })?;
    globals.raw_set("type", type_of)?;
    Ok(())
}

/// Builds a vector from constructor arguments. Numbers and vectors are
/// flattened in order, except that one number fills every component.
fn construct(name: &str, len: u8, args: MultiValue) -> mlua::Result<Vector> {
    let args = args.into_vec();
    if let [single] = args.as_slice() {
        if let Some(number) = number_of(single) {
            let mut c = [0.0; MAX_LEN];
            for slot in c.iter_mut().take(usize::from(len)) {
                *slot = number;
            }
            return Ok(Vector { len, c });
        }
    }

    let mut c = [0.0; MAX_LEN];
    let mut count = 0_usize;
    let mut push = |number: f32| {
        if let Some(slot) = c.get_mut(count) {
            *slot = number;
        }
        count += 1;
    };
    for arg in &args {
        if let Some(number) = number_of(arg) {
            push(number);
        } else if let Some(vector) = read(arg) {
            for component in vector.components() {
                push(*component);
            }
        } else {
            return Err(mlua::Error::runtime(format!(
                "{name}: expected numbers or vectors, got {}",
                arg.type_name()
            )));
        }
    }
    if count != usize::from(len) {
        return Err(mlua::Error::runtime(format!(
            "{name}: expected {len} components, got {count}"
        )));
    }
    Ok(Vector { len, c })
}

/// Applies `op` to two operands, where at least one is a vector.
fn arithmetic(op: Op, left: &LuaValue, right: &LuaValue) -> mlua::Result<Vector> {
    match (read(left), read(right)) {
        (Some(a), Some(b)) => {
            if a.len != b.len {
                return Err(mlua::Error::runtime(format!(
                    "vector size mismatch: {} and {}",
                    a.name(),
                    b.name()
                )));
            }
            Ok(Vector::from_fn(a.len, |index| {
                op.apply(a.c[index], b.c[index])
            }))
        }
        (Some(a), None) => {
            let number = number_of(right).ok_or_else(|| unsupported(op, left, right))?;
            Ok(Vector::from_fn(a.len, |index| op.apply(a.c[index], number)))
        }
        (None, Some(b)) => {
            let number = number_of(left).ok_or_else(|| unsupported(op, left, right))?;
            Ok(Vector::from_fn(b.len, |index| op.apply(number, b.c[index])))
        }
        (None, None) => Err(unsupported(op, left, right)),
    }
}

/// The error for an arithmetic operation that has no meaning for these operands.
fn unsupported(op: Op, left: &LuaValue, right: &LuaValue) -> mlua::Error {
    mlua::Error::runtime(format!(
        "cannot {} {} and {}",
        op.verb(),
        describe(left),
        describe(right)
    ))
}

/// The name of an operand for an error: a vector's name, or its Lua type.
fn describe(value: &LuaValue) -> String {
    match read(value) {
        Some(vector) => vector.name().to_owned(),
        None => value.type_name().to_owned(),
    }
}

/// Copies the vector out of a userdata. Returns `None` for any other value.
fn read(value: &LuaValue) -> Option<Vector> {
    match value {
        LuaValue::UserData(data) => data.borrow::<Vector>().ok().map(|vector| *vector),
        _ => None,
    }
}

/// The number a Lua value holds, as a component.
fn number_of(value: &LuaValue) -> Option<f32> {
    match value {
        LuaValue::Integer(int) => Some(int_to_f32(*int)),
        LuaValue::Number(float) => Some(float_to_f32(*float)),
        _ => None,
    }
}

/// Converts an integer to a component. Integers beyond 2^24 lose low bits, as they do in the games.
#[expect(
    clippy::cast_precision_loss,
    reason = "components are f32 like the games' vectors, so large integers round"
)]
fn int_to_f32(int: i64) -> f32 {
    int as f32
}

/// Converts a number to a component. Precision beyond `f32` is dropped, as the games do.
#[expect(
    clippy::cast_possible_truncation,
    reason = "components are f32 like the games' vectors, so precision is dropped"
)]
fn float_to_f32(float: f64) -> f32 {
    float as f32
}

/// `x`, `y`, `z` or `w` as a zero-based component index, if it is within `len`.
fn field_index(name: &[u8], len: usize) -> Option<usize> {
    let [letter] = name else {
        return None;
    };
    let index = LETTERS.iter().position(|known| known == letter)?;
    (index < len).then_some(index)
}

/// The zero-based index a Lua integer key names, which is `1..=len` in Lua terms.
fn integer_index(key: i64, len: usize) -> Option<usize> {
    let index = usize::try_from(key).ok()?.checked_sub(1)?;
    (index < len).then_some(index)
}

/// Reads a field: a component, a one-based integer, or a swizzle such as `zyx`.
fn index(lua: &Lua, this: &Vector, key: &LuaValue) -> mlua::Result<LuaValue> {
    let len = usize::from(this.len);
    let component = match key {
        LuaValue::Integer(int) => integer_index(*int, len).and_then(|index| this.at(index)),
        LuaValue::String(text) => {
            let bytes = text.as_bytes();
            match bytes.len() {
                1 => field_index(&bytes, len).and_then(|index| this.at(index)),
                2..=MAX_LEN => {
                    return match this.swizzle(&bytes) {
                        Some(vector) => Ok(LuaValue::UserData(lua.create_userdata(vector)?)),
                        None => Ok(LuaValue::Nil),
                    };
                }
                _ => None,
            }
        }
        _ => None,
    };
    Ok(component.map_or(LuaValue::Nil, |value| LuaValue::Number(f64::from(value))))
}

/// Sets a field: a component or a one-based integer, from a number.
fn assign(this: &mut Vector, key: &LuaValue, value: &LuaValue) -> mlua::Result<()> {
    let len = usize::from(this.len);
    let index = match key {
        LuaValue::Integer(int) => integer_index(*int, len),
        LuaValue::String(text) => field_index(&text.as_bytes(), len),
        _ => None,
    };
    let Some(index) = index else {
        return Err(mlua::Error::runtime(format!(
            "{} has no field {}",
            this.name(),
            describe_key(key)
        )));
    };
    let number = number_of(value).ok_or_else(|| {
        mlua::Error::runtime(format!(
            "{} component must be a number, got {}",
            this.name(),
            value.type_name()
        ))
    })?;
    if let Some(slot) = this.c.get_mut(index) {
        *slot = number;
    }
    Ok(())
}

/// A key for an error message.
fn describe_key(key: &LuaValue) -> String {
    match key {
        LuaValue::String(text) => format!("'{}'", text.to_string_lossy()),
        LuaValue::Integer(int) => int.to_string(),
        other => other.type_name().to_owned(),
    }
}
