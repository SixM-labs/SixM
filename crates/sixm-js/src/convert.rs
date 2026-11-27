//! Conversions between `QuickJS` values and [`sixm_core::Value`].

use rquickjs::convert::Coerced;
use rquickjs::{Array, ArrayBuffer, Ctx, Filter, FromJs, Object, TypedArray, Value as JsValue};
use sixm_core::{FuncRef, ScriptError, Value};

use crate::runtime::{JsRuntime, js_err};
use crate::vector::read_vector;

/// Whether functions may be converted into references.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Functions {
    /// Register them, for calls between resources.
    Share,
    /// Fail, for values sent over the network.
    Refuse,
}

/// Deepest object/array nesting allowed to cross a resource boundary.
pub(crate) const MAX_DEPTH: usize = 64;
/// Largest magnitude below which JS `number` represents every integer exactly (`2^53 - 1`).
pub(crate) const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// Returns `depth + 1`, or [`ScriptError::TooDeep`] past [`MAX_DEPTH`].
fn nested(depth: usize) -> Result<usize, ScriptError> {
    let depth = depth + 1;
    if depth > MAX_DEPTH {
        Err(ScriptError::TooDeep(MAX_DEPTH))
    } else {
        Ok(depth)
    }
}

/// Reads the raw bytes of a `Uint8Array` or `ArrayBuffer` without `unsafe`.
pub(crate) fn read_bytes(obj: &Object<'_>) -> Result<Option<Vec<u8>>, ScriptError> {
    if obj.is_typed_array::<u8>() {
        let ta = TypedArray::<u8>::from_object(obj.clone())
            .map_err(|err| ScriptError::Runtime(err.to_string()))?;
        if ta.as_raw().is_none() {
            return Err(ScriptError::Runtime(
                "Uint8Array buffer is detached".to_owned(),
            ));
        }
        let len = ta.len();
        let mut out = Vec::with_capacity(len);
        for index in 0..len {
            let idx = u32::try_from(index)
                .map_err(|_| ScriptError::Runtime("Uint8Array index overflow".to_owned()))?;
            let byte: u8 = ta
                .get(idx)
                .map_err(|err| ScriptError::Runtime(err.to_string()))?;
            out.push(byte);
        }
        return Ok(Some(out));
    }
    if obj.is_array_buffer() {
        let Some(ab) = ArrayBuffer::from_object(obj.clone()) else {
            return Err(ScriptError::Runtime("ArrayBuffer is detached".to_owned()));
        };
        let ta = TypedArray::<u8>::from_arraybuffer(ab)
            .map_err(|err| ScriptError::Runtime(err.to_string()))?;
        let len = ta.len();
        let mut out = Vec::with_capacity(len);
        for index in 0..len {
            let idx = u32::try_from(index)
                .map_err(|_| ScriptError::Runtime("ArrayBuffer index overflow".to_owned()))?;
            let byte: u8 = ta
                .get(idx)
                .map_err(|err| ScriptError::Runtime(err.to_string()))?;
            out.push(byte);
        }
        return Ok(Some(out));
    }
    Ok(None)
}

/// Converts raw bytes to a JS `string` if valid UTF-8, otherwise a `Uint8Array`.
pub(crate) fn bytes_to_js<'js>(ctx: &Ctx<'js>, bytes: &[u8]) -> Result<JsValue<'js>, ScriptError> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        rquickjs::String::from_str(ctx.clone(), text)
            .map(rquickjs::String::into_value)
            .map_err(|err| js_err(ctx, err))
    } else {
        TypedArray::<u8>::new_copy(ctx.clone(), bytes)
            .map(TypedArray::into_value)
            .map_err(|err| js_err(ctx, err))
    }
}

/// Converts JS arguments to [`Value`] allowing function references.
pub(crate) fn js_to_value_vec<'js>(
    runtime: &JsRuntime,
    ctx: &Ctx<'js>,
    values: &[JsValue<'js>],
) -> Result<Vec<Value>, ScriptError> {
    runtime.to_values(ctx, values)
}

/// Converts a slice of [`Value`] returned from `call_export` / `call_ref` into
/// a single JS return value following the cross-runtime convention:
/// `0` -> `undefined`, `1` -> `v`, `>1` -> JS `Array`.
pub(crate) fn values_to_js_return<'js>(
    runtime: &JsRuntime,
    ctx: &Ctx<'js>,
    values: &[Value],
) -> Result<JsValue<'js>, ScriptError> {
    match values {
        [] => Ok(JsValue::new_undefined(ctx.clone())),
        [single] => runtime.to_js(ctx, single),
        many => {
            let arr = Array::new(ctx.clone()).map_err(|err| js_err(ctx, err))?;
            for (index, item) in many.iter().enumerate() {
                arr.set(index, runtime.to_js(ctx, item)?)
                    .map_err(|err| js_err(ctx, err))?;
            }
            Ok(arr.into_value())
        }
    }
}

/// Checks if a JS value is an exact whole number in `[-MAX_SAFE_INTEGER, MAX_SAFE_INTEGER]`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "bounds check ensures f fits losslessly in i64"
)]
pub(crate) fn exact_safe_int(float: f64) -> Option<i64> {
    (float.is_finite() && float.fract() == 0.0 && float.abs() <= MAX_SAFE_INTEGER)
        .then_some(float as i64)
}

/// Checks if `obj` is a built-in container type other than plain Object/Array/TypedArray/ArrayBuffer.
fn is_disallowed_builtin<'js>(ctx: &Ctx<'js>, obj: &Object<'js>) -> bool {
    let globals = ctx.globals();
    for ctor_name in [
        "Map", "Set", "WeakMap", "WeakSet", "RegExp", "Date", "Error",
    ] {
        if let Ok(ctor) = globals.get::<_, JsValue<'_>>(ctor_name)
            && !ctor.is_undefined()
            && obj.is_instance_of(&ctor)
        {
            return true;
        }
    }
    false
}

impl JsRuntime {
    /// Converts JS values into script values (allowing functions).
    pub(crate) fn to_values<'js>(
        &self,
        ctx: &Ctx<'js>,
        values: &[JsValue<'js>],
    ) -> Result<Vec<Value>, ScriptError> {
        values
            .iter()
            .map(|value| self.to_value_at(ctx, value, 0, Functions::Share, &mut Vec::new()))
            .collect()
    }

    /// Converts JS values bound for the network (refusing functions).
    pub(crate) fn to_network_values<'js>(
        &self,
        ctx: &Ctx<'js>,
        values: &[JsValue<'js>],
    ) -> Result<Vec<Value>, ScriptError> {
        values
            .iter()
            .map(|value| self.to_network_value(ctx, value))
            .collect()
    }

    /// Converts a single JS value bound for the network (refusing functions).
    pub(crate) fn to_network_value<'js>(
        &self,
        ctx: &Ctx<'js>,
        value: &JsValue<'js>,
    ) -> Result<Value, ScriptError> {
        self.to_value_at(ctx, value, 0, Functions::Refuse, &mut Vec::new())
    }

    /// Converts a single JS value into a [`Value`] (allowing functions).
    pub(crate) fn to_value<'js>(
        &self,
        ctx: &Ctx<'js>,
        value: &JsValue<'js>,
    ) -> Result<Value, ScriptError> {
        self.to_value_at(ctx, value, 0, Functions::Share, &mut Vec::new())
    }

    /// Converts the return value of a JS export or ref callback into `Vec<Value>`.
    /// `undefined` becomes `vec![]`, every other value becomes `vec![val]`.
    pub(crate) fn js_return_to_values<'js>(
        &self,
        ctx: &Ctx<'js>,
        value: &JsValue<'js>,
    ) -> Result<Vec<Value>, ScriptError> {
        if value.is_undefined() {
            Ok(Vec::new())
        } else {
            Ok(vec![self.to_value(ctx, value)?])
        }
    }

    fn to_value_at<'js>(
        &self,
        ctx: &Ctx<'js>,
        value: &JsValue<'js>,
        depth: usize,
        functions: Functions,
        ancestors: &mut Vec<JsValue<'js>>,
    ) -> Result<Value, ScriptError> {
        if value.is_null() || value.is_undefined() {
            return Ok(Value::Nil);
        }
        if let Some(flag) = value.as_bool() {
            return Ok(Value::Bool(flag));
        }
        if let Some(int) = value.as_int() {
            return Ok(Value::Int(i64::from(int)));
        }
        if let Some(float) = value.as_float() {
            return Ok(exact_safe_int(float).map_or(Value::Float(float), Value::Int));
        }
        if let Some(bigint) = value.as_big_int() {
            let text = Coerced::<String>::from_js(ctx, value.clone())
                .map_err(|err| js_err(ctx, err))?
                .0;
            let int = bigint.clone().to_i64().map_err(|err| js_err(ctx, err))?;
            if int.to_string() != text {
                return Err(ScriptError::Runtime(
                    "BigInt value is outside the signed 64-bit integer range".to_owned(),
                ));
            }
            return Ok(Value::Int(int));
        }
        if let Some(text) = value.as_string() {
            let text = text.to_string().map_err(|err| js_err(ctx, err))?;
            return Ok(Value::from(text));
        }
        if let Some(vector) = read_vector(value) {
            return Ok(vector.to_value());
        }
        if value.is_promise() {
            return Err(ScriptError::Runtime(
                "cannot pass a promise value to another resource".to_owned(),
            ));
        }
        if value.is_symbol() {
            return Err(ScriptError::Runtime(
                "cannot pass a symbol value to another resource".to_owned(),
            ));
        }
        if let Some(func) = value.as_function() {
            if functions == Functions::Refuse {
                return Err(ScriptError::Runtime(
                    "functions cannot be sent over the network".to_owned(),
                ));
            }
            if let Some(foreign) = self.unwrap_foreign_func(ctx, func) {
                return Ok(Value::FuncRef(foreign));
            }
            return Ok(Value::FuncRef(self.ref_function(ctx, func.clone())?));
        }
        if let Some(arr) = value.as_array() {
            return self.array_to_value(ctx, value, arr, depth, functions, ancestors);
        }
        if let Some(obj) = value.as_object() {
            return self.object_to_value(ctx, value, obj, depth, functions, ancestors);
        }
        Err(ScriptError::Runtime(format!(
            "cannot pass a {} value to another resource",
            value.type_name()
        )))
    }

    fn array_to_value<'js>(
        &self,
        ctx: &Ctx<'js>,
        value: &JsValue<'js>,
        arr: &Array<'js>,
        depth: usize,
        functions: Functions,
        ancestors: &mut Vec<JsValue<'js>>,
    ) -> Result<Value, ScriptError> {
        if ancestors.iter().any(|seen| seen == value) {
            return Err(ScriptError::TooDeep(MAX_DEPTH));
        }
        let next_depth = nested(depth)?;
        ancestors.push(value.clone());
        let mut items = Vec::with_capacity(arr.len());
        for index in 0..arr.len() {
            let item: JsValue<'_> = match arr.get(index) {
                Ok(item) => item,
                Err(err) => {
                    ancestors.pop();
                    return Err(js_err(ctx, err));
                }
            };
            match self.to_value_at(ctx, &item, next_depth, functions, ancestors) {
                Ok(converted) => items.push(converted),
                Err(err) => {
                    ancestors.pop();
                    return Err(err);
                }
            }
        }
        ancestors.pop();
        Ok(Value::Array(items))
    }

    fn object_to_value<'js>(
        &self,
        ctx: &Ctx<'js>,
        value: &JsValue<'js>,
        obj: &Object<'js>,
        depth: usize,
        functions: Functions,
        ancestors: &mut Vec<JsValue<'js>>,
    ) -> Result<Value, ScriptError> {
        if let Some(bytes) = read_bytes(obj)? {
            return Ok(Value::String(bytes.into_boxed_slice()));
        }
        if value.is_proxy() || is_disallowed_builtin(ctx, obj) {
            return Err(ScriptError::Runtime(format!(
                "cannot pass a {} value to another resource",
                value.type_name()
            )));
        }
        // Reject non-plain JS class instances (prototype is neither Object.prototype nor null).
        if let Some(proto) = obj.get_prototype() {
            let obj_proto: Option<Object<'_>> = ctx
                .globals()
                .get::<_, Object<'_>>("Object")
                .ok()
                .and_then(|ctor| ctor.get("prototype").ok());
            if obj_proto.as_ref().is_some_and(|op| *op != proto) {
                return Err(ScriptError::Runtime(
                    "cannot pass a userdata value to another resource".to_owned(),
                ));
            }
        }
        if ancestors.iter().any(|seen| seen == value) {
            return Err(ScriptError::TooDeep(MAX_DEPTH));
        }
        let next_depth = nested(depth)?;
        ancestors.push(value.clone());
        let mut pairs = Vec::new();
        let keys: Vec<String> = obj
            .own_keys::<String>(Filter::new().string().enum_only())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| {
                ancestors.pop();
                js_err(ctx, err)
            })?;
        for key in keys {
            let prop_val: JsValue<'_> = match obj.get(&key) {
                Ok(v) => v,
                Err(err) => {
                    ancestors.pop();
                    return Err(js_err(ctx, err));
                }
            };
            match self.to_value_at(ctx, &prop_val, next_depth, functions, ancestors) {
                Ok(converted) => pairs.push((Value::from(key), converted)),
                Err(err) => {
                    ancestors.pop();
                    return Err(err);
                }
            }
        }
        ancestors.pop();
        Ok(Value::Map(pairs))
    }

    /// Converts a list of [`Value`] into JS values.
    pub(crate) fn values_to_js<'js>(
        &self,
        ctx: &Ctx<'js>,
        values: &[Value],
    ) -> Result<Vec<JsValue<'js>>, ScriptError> {
        values.iter().map(|val| self.to_js(ctx, val)).collect()
    }

    /// Converts a single [`Value`] into a JS value.
    pub(crate) fn to_js<'js>(
        &self,
        ctx: &Ctx<'js>,
        value: &Value,
    ) -> Result<JsValue<'js>, ScriptError> {
        self.to_js_at(ctx, value, 0)
    }

    fn to_js_at<'js>(
        &self,
        ctx: &Ctx<'js>,
        value: &Value,
        depth: usize,
    ) -> Result<JsValue<'js>, ScriptError> {
        Ok(match value {
            Value::Nil => JsValue::new_null(ctx.clone()),
            Value::Bool(flag) => JsValue::new_bool(ctx.clone(), *flag),
            Value::Int(int) => int_to_js(ctx, *int),
            Value::Float(float) => JsValue::new_float(ctx.clone(), *float),
            Value::String(bytes) => bytes_to_js(ctx, bytes)?,
            Value::Array(items) => {
                let next_depth = nested(depth)?;
                let arr = Array::new(ctx.clone()).map_err(|err| js_err(ctx, err))?;
                for (index, item) in items.iter().enumerate() {
                    arr.set(index, self.to_js_at(ctx, item, next_depth)?)
                        .map_err(|err| js_err(ctx, err))?;
                }
                arr.into_value()
            }
            Value::Map(pairs) => {
                let next_depth = nested(depth)?;
                let obj = Object::new(ctx.clone()).map_err(|err| js_err(ctx, err))?;
                for (key, item) in pairs {
                    let key_str = map_key_string(key);
                    obj.set(key_str, self.to_js_at(ctx, item, next_depth)?)
                        .map_err(|err| js_err(ctx, err))?;
                }
                obj.into_value()
            }
            Value::Vector2(c) => {
                crate::vector::make_vector(ctx, *c).map_err(|err| js_err(ctx, err))?
            }
            Value::Vector3(c) => {
                crate::vector::make_vector(ctx, *c).map_err(|err| js_err(ctx, err))?
            }
            Value::Vector4(c) => {
                crate::vector::make_vector(ctx, *c).map_err(|err| js_err(ctx, err))?
            }
            Value::FuncRef(func) => self.js_function(ctx, func)?,
        })
    }

    fn js_function<'js>(
        &self,
        ctx: &Ctx<'js>,
        func: &FuncRef,
    ) -> Result<JsValue<'js>, ScriptError> {
        if func.owner == self.resource && func.instance == self.instance {
            let persistent = self
                .lookup_function(func.id)
                .ok_or_else(|| ScriptError::StaleRef(func.clone()))?;
            let function = persistent.restore(ctx).map_err(|err| js_err(ctx, err))?;
            return Ok(function.into_value());
        }
        self.foreign_js_function(ctx, func)
    }
}

/// Converts an `i64` to a `QuickJS` number (`i32` when it fits, else `f64`).
#[expect(
    clippy::cast_precision_loss,
    reason = "JavaScript numbers are IEEE-754 f64; integers beyond 2^53-1 round"
)]
pub(crate) fn int_to_js<'js>(ctx: &Ctx<'js>, int: i64) -> JsValue<'js> {
    if let Ok(small) = i32::try_from(int) {
        JsValue::new_int(ctx.clone(), small)
    } else {
        JsValue::new_float(ctx.clone(), int as f64)
    }
}

fn map_key_string(key: &Value) -> String {
    match key {
        Value::String(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        Value::Int(int) => int.to_string(),
        Value::Float(float) => float.to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Nil => "null".to_owned(),
        other => format!("{other:?}"),
    }
}
