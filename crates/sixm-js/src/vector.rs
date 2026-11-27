//! The `vector2`, `vector3` and `vector4` types for JavaScript.
//!
//! Vectors are wrapped in an ES6 `Proxy` over a [`JsVector`] class so both
//! Lua-style properties (`v.x`, `v.y`, `v.z`, `v.w`, swizzles like `v.xy`)
//! and JS-style indexing / destructuring (`v[0]`, `const [x, y, z] = v`,
//! `v.length`, `v.toString()`, `JSON.stringify(v)`) work on the same value.

use std::fmt;

use rquickjs::class::Trace;
use rquickjs::function::Rest;
use rquickjs::proxy::{Proxy, ProxyHandler, ProxyProperty, ProxyTarget};
use rquickjs::{
    Array, Class, Ctx, Exception, Function, JsLifetime, Object, Symbol, Value as JsValue,
};
use sixm_core::Value;

/// Largest number of components (`vector4`).
const MAX_LEN: usize = 4;
const LETTERS: &[u8; MAX_LEN] = b"xyzw";
const NAMES: [&str; MAX_LEN] = ["x", "y", "z", "w"];
const QUAT_LETTERS: &[u8; MAX_LEN] = b"wxyz";
const QUAT_NAMES: [&str; MAX_LEN] = ["w", "x", "y", "z"];

/// Underlying class stored behind each vector `Proxy`.
#[derive(Debug, Clone, Copy, PartialEq, Trace, JsLifetime)]
#[rquickjs::class]
pub(crate) struct JsVector {
    #[qjs(skip_trace)]
    len: u8,
    #[qjs(skip_trace)]
    c: [f32; MAX_LEN],
}

impl JsVector {
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

    pub(crate) fn components(&self) -> &[f32] {
        &self.c[..usize::from(self.len)]
    }

    pub(crate) fn name(&self) -> &'static str {
        match self.len {
            2 => "vector2",
            3 => "vector3",
            _ => "vector4",
        }
    }

    fn at(&self, index: usize) -> Option<f32> {
        if index < usize::from(self.len) {
            self.c.get(index).copied()
        } else {
            None
        }
    }

    fn field(&self, letter: u8) -> Option<f32> {
        let index = LETTERS.iter().position(|known| *known == letter)?;
        self.at(index)
    }

    fn swizzle(&self, letters: &[u8]) -> Option<Self> {
        let len = u8::try_from(letters.len()).ok()?;
        let mut c = [0.0; MAX_LEN];
        for (slot, letter) in c.iter_mut().zip(letters) {
            *slot = self.field(*letter)?;
        }
        Some(Self { len, c })
    }

    pub(crate) fn to_value(self) -> Value {
        match self.len {
            2 => Value::Vector2([self.c[0], self.c[1]]),
            3 => Value::Vector3([self.c[0], self.c[1], self.c[2]]),
            _ => Value::Vector4(self.c),
        }
    }
}

impl fmt::Display for JsVector {
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

/// Creates a JS `Proxy` wrapping a new [`JsVector`].
pub(crate) fn make_vector<'js, const N: usize>(
    ctx: &Ctx<'js>,
    components: [f32; N],
) -> rquickjs::Result<JsValue<'js>> {
    wrap_vector(ctx, JsVector::new(components))
}

fn wrap_vector<'js>(ctx: &Ctx<'js>, vector: JsVector) -> rquickjs::Result<JsValue<'js>> {
    let target = Class::instance(ctx.clone(), vector)?;
    let handler = ProxyHandler::new(ctx.clone())?
        .with_getter(vector_get)?
        .with_setter(vector_set)?
        .with_has(vector_has)?;

    let proxy = Proxy::new(ctx.clone(), target, handler)?;
    Ok(proxy.into_value())
}

// rquickjs::FromJs is only implemented for owned values, so proxy trap params are passed by value.
#[allow(clippy::needless_pass_by_value)]
fn vector_get<'js>(
    target: ProxyTarget<'js>,
    prop: ProxyProperty<'js>,
    _recv: rquickjs::proxy::ProxyReceiver<'js>,
) -> rquickjs::Result<JsValue<'js>> {
    let ctx = target.0.ctx().clone();
    let cls = target
        .0
        .as_class::<JsVector>()
        .ok_or_else(|| Exception::throw_type(&ctx, "expected vector"))?;
    let vec = *cls.borrow();

    if prop.is_symbol() {
        if prop.0 == *Symbol::iterator(ctx.clone()).as_value() {
            return make_iterator_fn(&ctx, vec).map(Function::into_value);
        }
        return Ok(JsValue::new_undefined(ctx));
    }

    let Ok(key) = prop.to_string() else {
        return Ok(JsValue::new_undefined(ctx));
    };

    match key.as_str() {
        "length" => return Ok(JsValue::new_int(ctx, i32::from(vec.len))),
        "toString" => {
            let text = vec.to_string();
            return Function::new(ctx, move || text.clone()).map(Function::into_value);
        }
        "toJSON" => {
            return Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>| -> rquickjs::Result<Object<'js>> {
                    let obj = Object::new(ctx)?;
                    for (name, comp) in NAMES.iter().zip(vec.components()) {
                        obj.set(*name, f64::from(*comp))?;
                    }
                    Ok(obj)
                },
            )
            .map(Function::into_value);
        }
        _ => {}
    }

    if let Some(index) = parse_zero_index(&key, usize::from(vec.len)) {
        return Ok(vec.at(index).map_or_else(
            || JsValue::new_undefined(ctx.clone()),
            |v| JsValue::new_float(ctx.clone(), f64::from(v)),
        ));
    }

    let bytes = key.as_bytes();
    match bytes.len() {
        1 => {
            let val = field_index(bytes, usize::from(vec.len)).and_then(|i| vec.at(i));
            Ok(val.map_or_else(
                || JsValue::new_undefined(ctx.clone()),
                |v| JsValue::new_float(ctx.clone(), f64::from(v)),
            ))
        }
        2..=MAX_LEN => match vec.swizzle(bytes) {
            Some(swizzled) => wrap_vector(&ctx, swizzled),
            None => Ok(JsValue::new_undefined(ctx)),
        },
        _ => Ok(JsValue::new_undefined(ctx)),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn vector_set<'js>(
    target: ProxyTarget<'js>,
    prop: ProxyProperty<'js>,
    value: JsValue<'js>,
    _recv: rquickjs::proxy::ProxyReceiver<'js>,
) -> rquickjs::Result<bool> {
    let ctx = target.0.ctx().clone();
    let cls = target
        .0
        .as_class::<JsVector>()
        .ok_or_else(|| Exception::throw_type(&ctx, "expected vector"))?;
    let mut vec = cls.borrow_mut();
    let len = usize::from(vec.len);

    let key = prop
        .to_string()
        .map_err(|_| Exception::throw_type(&ctx, &format!("{} has no such field", vec.name())))?;
    let index = parse_zero_index(&key, len)
        .or_else(|| field_index(key.as_bytes(), len))
        .ok_or_else(|| {
            Exception::throw_type(&ctx, &format!("{} has no field '{key}'", vec.name()))
        })?;
    let num = number_of(&value).ok_or_else(|| {
        Exception::throw_type(
            &ctx,
            &format!(
                "{} component must be a number, got {}",
                vec.name(),
                value.type_name()
            ),
        )
    })?;
    if let Some(slot) = vec.c.get_mut(index) {
        *slot = num;
    }
    Ok(true)
}

#[allow(clippy::needless_pass_by_value)]
fn vector_has<'js>(target: ProxyTarget<'js>, prop: ProxyProperty<'js>) -> rquickjs::Result<bool> {
    let ctx = target.0.ctx().clone();
    let cls = target
        .0
        .as_class::<JsVector>()
        .ok_or_else(|| Exception::throw_type(&ctx, "expected vector"))?;
    let vec = *cls.borrow();
    if prop.is_symbol() {
        return Ok(prop.0 == *Symbol::iterator(ctx).as_value());
    }
    let Ok(key) = prop.to_string() else {
        return Ok(false);
    };
    let len = usize::from(vec.len);
    Ok(matches!(key.as_str(), "length" | "toString" | "toJSON")
        || parse_zero_index(&key, len).is_some()
        || field_index(key.as_bytes(), len).is_some())
}

fn make_iterator_fn<'js>(ctx: &Ctx<'js>, vec: JsVector) -> rquickjs::Result<Function<'js>> {
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>| -> rquickjs::Result<Object<'js>> {
            let components: Vec<f64> = vec.components().iter().map(|c| f64::from(*c)).collect();
            let arr = Array::new(ctx.clone())?;
            for (i, c) in components.iter().enumerate() {
                arr.set(i, *c)?;
            }
            let values_fn: Function<'js> = Object::get(&arr, Symbol::iterator(ctx))?;
            values_fn.call((rquickjs::function::This(arr),))
        },
    )
}

/// Reads a [`JsVector`] out of either a `Proxy` or a raw `Class<JsVector>`.
pub(crate) fn read_vector(value: &JsValue<'_>) -> Option<JsVector> {
    if let Some(proxy) = value.as_proxy() {
        let target = proxy.target().ok()?;
        let cls = target.as_class::<JsVector>()?;
        return Some(*cls.borrow());
    }
    value
        .as_object()
        .and_then(|obj| obj.as_class::<JsVector>())
        .map(|cls| *cls.borrow())
}

/// Converts a JS number to `f32`, dropping precision beyond 24 bits like the games do.
pub(crate) fn number_of(value: &JsValue<'_>) -> Option<f32> {
    if let Some(int) = value.as_int() {
        Some(int_to_f32(i64::from(int)))
    } else {
        value.as_float().map(float_to_f32)
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "vector components are f32 like the games' vectors"
)]
fn int_to_f32(int: i64) -> f32 {
    int as f32
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "vector components are f32 like the games' vectors"
)]
fn float_to_f32(float: f64) -> f32 {
    float as f32
}

fn field_index(name: &[u8], len: usize) -> Option<usize> {
    let [letter] = name else {
        return None;
    };
    let index = LETTERS.iter().position(|known| known == letter)?;
    (index < len).then_some(index)
}

fn parse_zero_index(key: &str, len: usize) -> Option<usize> {
    match key {
        "0" if len > 0 => Some(0),
        "1" if len > 1 => Some(1),
        "2" if len > 2 => Some(2),
        "3" if len > 3 => Some(3),
        _ => None,
    }
}

/// Installs `vector2`, `vec2`, `vector3`, `vec3`, `vector4`, `vec4` constructors.
pub(crate) fn install<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let constructors: [(&'static str, u8); 6] = [
        ("vector2", 2),
        ("vec2", 2),
        ("vector3", 3),
        ("vec3", 3),
        ("vector4", 4),
        ("vec4", 4),
    ];
    for (name, len) in constructors {
        let func = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, args: Rest<JsValue<'js>>| -> rquickjs::Result<JsValue<'js>> {
                construct(&ctx, name, len, &args.0)
            },
        )?;
        globals.set(name, func)?;
    }
    globals.set(
        "quat",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, args: Rest<JsValue<'js>>| -> rquickjs::Result<JsValue<'js>> {
                construct_quat(&ctx, &args.0)
            },
        )?,
    )?;
    Ok(())
}

fn construct<'js>(
    ctx: &Ctx<'js>,
    name: &str,
    len: u8,
    args: &[JsValue<'js>],
) -> rquickjs::Result<JsValue<'js>> {
    if let [single] = args
        && let Some(num) = number_of(single)
    {
        let mut c = [0.0; MAX_LEN];
        for slot in c.iter_mut().take(usize::from(len)) {
            *slot = num;
        }
        return wrap_vector(ctx, JsVector { len, c });
    }

    let mut c = [0.0; MAX_LEN];
    let mut count = 0_usize;
    let mut push = |num: f32| {
        if let Some(slot) = c.get_mut(count) {
            *slot = num;
        }
        count += 1;
    };
    for arg in args {
        if let Some(num) = number_of(arg) {
            push(num);
        } else if let Some(vec) = read_vector(arg) {
            for comp in vec.components() {
                push(*comp);
            }
        } else {
            return Err(Exception::throw_type(
                ctx,
                &format!(
                    "{name}: expected numbers or vectors, got {}",
                    arg.type_name()
                ),
            ));
        }
    }
    if count != usize::from(len) {
        return Err(Exception::throw_range(
            ctx,
            &format!("{name}: expected {len} components, got {count}"),
        ));
    }
    wrap_vector(ctx, JsVector { len, c })
}

/// Underlying class stored behind each quaternion `Proxy`, in `[w, x, y, z]` order.
#[derive(Debug, Clone, Copy, PartialEq, Trace, JsLifetime)]
#[rquickjs::class]
pub(crate) struct JsQuat {
    #[qjs(skip_trace)]
    c: [f32; 4],
}

impl JsQuat {
    pub(crate) fn new(c: [f32; 4]) -> Self {
        Self { c }
    }

    pub(crate) fn to_value(self) -> Value {
        Value::Quat(self.c)
    }
}

impl fmt::Display for JsQuat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "quat({:?}, {:?}, {:?}, {:?})",
            self.c[0], self.c[1], self.c[2], self.c[3]
        )
    }
}

pub(crate) fn make_quat<'js>(ctx: &Ctx<'js>, c: [f32; 4]) -> rquickjs::Result<JsValue<'js>> {
    wrap_quat(ctx, JsQuat::new(c))
}

fn wrap_quat<'js>(ctx: &Ctx<'js>, q: JsQuat) -> rquickjs::Result<JsValue<'js>> {
    let target = Class::instance(ctx.clone(), q)?;
    let handler = ProxyHandler::new(ctx.clone())?
        .with_getter(quat_get)?
        .with_setter(quat_set)?
        .with_has(quat_has)?;
    Ok(Proxy::new(ctx.clone(), target, handler)?.into_value())
}

pub(crate) fn read_quat(value: &JsValue<'_>) -> Option<JsQuat> {
    if let Some(proxy) = value.as_proxy() {
        let target = proxy.target().ok()?;
        let cls = target.as_class::<JsQuat>()?;
        return Some(*cls.borrow());
    }
    value
        .as_object()
        .and_then(|obj| obj.as_class::<JsQuat>())
        .map(|cls| *cls.borrow())
}

fn quat_field_index(key: &str) -> Option<usize> {
    if let Some(idx) = parse_zero_index(key, 4) {
        return Some(idx);
    }
    let [letter] = key.as_bytes() else {
        return None;
    };
    QUAT_LETTERS.iter().position(|known| known == letter)
}

#[allow(clippy::needless_pass_by_value)]
fn quat_get<'js>(
    target: ProxyTarget<'js>,
    prop: ProxyProperty<'js>,
    _recv: rquickjs::proxy::ProxyReceiver<'js>,
) -> rquickjs::Result<JsValue<'js>> {
    let ctx = target.0.ctx().clone();
    let cls = target
        .0
        .as_class::<JsQuat>()
        .ok_or_else(|| Exception::throw_type(&ctx, "expected quat"))?;
    let q = *cls.borrow();

    if prop.is_symbol() {
        if prop.0 == *Symbol::iterator(ctx.clone()).as_value() {
            return make_slice_iterator(&ctx, &q.c).map(Function::into_value);
        }
        return Ok(JsValue::new_undefined(ctx));
    }

    let Ok(key) = prop.to_string() else {
        return Ok(JsValue::new_undefined(ctx));
    };

    match key.as_str() {
        "length" => return Ok(JsValue::new_int(ctx, 4)),
        "toString" => {
            let text = q.to_string();
            return Function::new(ctx, move || text.clone()).map(Function::into_value);
        }
        "toJSON" => {
            return Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>| -> rquickjs::Result<Object<'js>> {
                    let obj = Object::new(ctx)?;
                    for (name, comp) in QUAT_NAMES.iter().zip(q.c) {
                        obj.set(*name, f64::from(comp))?;
                    }
                    Ok(obj)
                },
            )
            .map(Function::into_value);
        }
        _ => {}
    }

    Ok(quat_field_index(&key).map_or_else(
        || JsValue::new_undefined(ctx.clone()),
        |idx| JsValue::new_float(ctx.clone(), f64::from(q.c[idx])),
    ))
}

#[allow(clippy::needless_pass_by_value)]
fn quat_set<'js>(
    target: ProxyTarget<'js>,
    prop: ProxyProperty<'js>,
    value: JsValue<'js>,
    _recv: rquickjs::proxy::ProxyReceiver<'js>,
) -> rquickjs::Result<bool> {
    let ctx = target.0.ctx().clone();
    let cls = target
        .0
        .as_class::<JsQuat>()
        .ok_or_else(|| Exception::throw_type(&ctx, "expected quat"))?;
    let mut q = cls.borrow_mut();
    let key = prop
        .to_string()
        .map_err(|_| Exception::throw_type(&ctx, "quat has no such field"))?;
    let idx = quat_field_index(&key)
        .ok_or_else(|| Exception::throw_type(&ctx, &format!("quat has no field '{key}'")))?;
    let num = number_of(&value).ok_or_else(|| {
        Exception::throw_type(
            &ctx,
            &format!("quat component must be a number, got {}", value.type_name()),
        )
    })?;
    q.c[idx] = num;
    Ok(true)
}

#[allow(clippy::needless_pass_by_value)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "ProxyHandler::with_has requires rquickjs::Result<bool>"
)]
fn quat_has<'js>(target: ProxyTarget<'js>, prop: ProxyProperty<'js>) -> rquickjs::Result<bool> {
    let ctx = target.0.ctx().clone();
    if prop.is_symbol() {
        return Ok(prop.0 == *Symbol::iterator(ctx).as_value());
    }
    let Ok(key) = prop.to_string() else {
        return Ok(false);
    };
    Ok(
        matches!(key.as_str(), "length" | "toString" | "toJSON")
            || quat_field_index(&key).is_some(),
    )
}

fn make_slice_iterator<'js>(ctx: &Ctx<'js>, slice: &[f32]) -> rquickjs::Result<Function<'js>> {
    let components: Vec<f64> = slice.iter().map(|c| f64::from(*c)).collect();
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>| -> rquickjs::Result<Object<'js>> {
            let arr = Array::new(ctx.clone())?;
            for (i, c) in components.iter().enumerate() {
                arr.set(i, *c)?;
            }
            let values_fn: Function<'js> = Object::get(&arr, Symbol::iterator(ctx))?;
            values_fn.call((rquickjs::function::This(arr),))
        },
    )
}

fn construct_quat<'js>(ctx: &Ctx<'js>, args: &[JsValue<'js>]) -> rquickjs::Result<JsValue<'js>> {
    match args {
        [single] => {
            if let Some(q) = read_quat(single) {
                return wrap_quat(ctx, q);
            }
            if let Some(n) = number_of(single) {
                return wrap_quat(ctx, JsQuat::new([n, n, n, n]));
            }
        }
        [w, v] => {
            if let (Some(w), Some(vec)) = (number_of(w), read_vector(v))
                && vec.len == 3
            {
                return wrap_quat(ctx, JsQuat::new([w, vec.c[0], vec.c[1], vec.c[2]]));
            }
        }
        [w, x, y, z] => {
            if let (Some(w), Some(x), Some(y), Some(z)) =
                (number_of(w), number_of(x), number_of(y), number_of(z))
            {
                return wrap_quat(ctx, JsQuat::new([w, x, y, z]));
            }
        }
        _ => {}
    }
    Err(Exception::throw_type(
        ctx,
        "quat: expected (w, x, y, z), (w, vector3), or (quat)",
    ))
}
