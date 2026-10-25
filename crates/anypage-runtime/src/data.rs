//! The one value type scripts and native operators exchange.
//!
//! Script functions never see engine types. Their inputs arrive as `Data`
//! converted to Luau values, and whatever they return is converted back, so a
//! script result can be compared (early cutoff), cached, and handed to native
//! operators on other threads.
//!
//! Two kinds of value never get copied into the script VM:
//! * [`Data::Bytes`] travels as a Luau `buffer` (copied once at the boundary);
//! * [`Res`] is a handle to a native object (a PDF document, a workbook, a
//!   decoded image, a mesh). Scripts hold it as userdata and pass it back to
//!   native functions; `res:method(...)` calls `module.method(res, ...)`.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anypage_core::{Input, Status};
use mlua::{AnyUserData, Lua, MetaMethod, Table, UserData, UserDataMethods, Value as LuaValue};
use serde_json::Value as Json;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Data {
    #[default]
    Nil,
    Bool(bool),
    Num(f64),
    Str(Arc<str>),
    Bytes(Arc<Vec<u8>>),
    List(Arc<Vec<Data>>),
    Map(Arc<BTreeMap<Arc<str>, Data>>),
    Res(Res),
}

/// A value produced from a stream, stamped with when its newest item was
/// produced, so the engine can measure item-to-screen latency through scripts.
#[derive(Clone, Debug, PartialEq)]
pub struct Streamed {
    pub data: Data,
    pub newest: Option<Instant>,
}

static NEXT_RES: AtomicU64 = AtomicU64::new(1);

/// Handle to a native object. Equality is identity plus version, so a
/// resource mutated in place (a workbook after an edit) is published as
/// `next_version()` and everything that read the old version recomputes.
#[derive(Clone)]
pub struct Res {
    kind: Arc<str>,
    id: u64,
    version: u64,
    obj: Arc<dyn Any + Send + Sync>,
}

impl Res {
    /// `kind` is "module.type", e.g. "pdf.document"; `res:f()` resolves `f` in
    /// the global table named by the part before the dot.
    pub fn new<T: Any + Send + Sync>(kind: &str, obj: T) -> Res {
        Self::from_arc(kind, Arc::new(obj))
    }
    pub fn from_arc(kind: &str, obj: Arc<dyn Any + Send + Sync>) -> Res {
        Res { kind: Arc::from(kind), id: NEXT_RES.fetch_add(1, Ordering::Relaxed), version: 0, obj }
    }
    pub fn kind(&self) -> &str {
        &self.kind
    }
    pub fn module(&self) -> &str {
        self.kind.split('.').next().unwrap_or("")
    }
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn version(&self) -> u64 {
        self.version
    }
    pub fn get<T: Any>(&self) -> Option<&T> {
        self.obj.downcast_ref::<T>()
    }
    pub fn arc(&self) -> &Arc<dyn Any + Send + Sync> {
        &self.obj
    }
    pub fn downcast_arc<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.obj.clone().downcast::<T>().ok()
    }
    /// The same object at a given version (a document's own revision counter).
    pub fn at_version(&self, version: u64) -> Res {
        Res { kind: self.kind.clone(), id: self.id, version, obj: self.obj.clone() }
    }
    /// The same object, published as changed.
    pub fn next_version(&self) -> Res {
        Res { kind: self.kind.clone(), id: self.id, version: self.version + 1, obj: self.obj.clone() }
    }
}

impl PartialEq for Res {
    fn eq(&self, o: &Self) -> bool {
        self.id == o.id && self.version == o.version && self.kind == o.kind
    }
}

impl fmt::Debug for Res {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}v{}", self.kind, self.id, self.version)
    }
}

/// How a [`Res`] looks inside the VM.
pub struct ResUd(pub Res);

impl UserData for ResUd {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::ToString, |_, this, ()| Ok(format!("{:?}", this.0)));
        // #bytes for byte resources (files, assets)
        m.add_meta_method(MetaMethod::Len, |_, this, ()| match this.0.get::<Vec<u8>>() {
            Some(b) => Ok(b.len() as f64),
            None => Err(mlua::Error::runtime(format!("# is defined for byte resources, not {}", this.0.kind()))),
        });
        m.add_meta_method(MetaMethod::Eq, |_, this, other: AnyUserData| Ok(other.borrow::<ResUd>().is_ok_and(|o| o.0 == this.0)));
        // res:method(...) looks the method up in the module table of its kind
        m.add_meta_method(MetaMethod::Index, |lua, this, key: String| {
            if key == "kind" {
                return Ok(LuaValue::String(lua.create_string(this.0.kind())?));
            }
            let module: Option<Table> = lua.globals().get(this.0.module())?;
            match module {
                Some(t) => t.get::<LuaValue>(key),
                None => Ok(LuaValue::Nil),
            }
        });
    }
}

const MAX_DEPTH: u32 = 32;

impl Data {
    pub fn str(s: &str) -> Data {
        Data::Str(Arc::from(s))
    }
    pub fn list(v: Vec<Data>) -> Data {
        Data::List(Arc::new(v))
    }
    pub fn map<'a>(pairs: impl IntoIterator<Item = (&'a str, Data)>) -> Data {
        Data::Map(Arc::new(pairs.into_iter().map(|(k, v)| (Arc::from(k), v)).collect()))
    }
    pub fn nums(v: impl IntoIterator<Item = f64>) -> Data {
        Data::list(v.into_iter().map(Data::Num).collect())
    }
    pub fn bytes(v: Vec<u8>) -> Data {
        Data::Bytes(Arc::new(v))
    }
    pub fn res<T: Any + Send + Sync>(kind: &str, obj: T) -> Data {
        Data::Res(Res::new(kind, obj))
    }

    pub fn get(&self, key: &str) -> &Data {
        static NIL: Data = Data::Nil;
        match self {
            Data::Map(m) => m.get(key).unwrap_or(&NIL),
            _ => &NIL,
        }
    }
    /// List element, 1-based like Luau.
    pub fn at(&self, i: usize) -> &Data {
        static NIL: Data = Data::Nil;
        self.items().get(i.wrapping_sub(1)).unwrap_or(&NIL)
    }
    pub fn is_nil(&self) -> bool {
        matches!(self, Data::Nil)
    }
    pub fn num(&self) -> Option<f64> {
        match self {
            Data::Num(n) => Some(*n),
            _ => None,
        }
    }
    pub fn num_or(&self, d: f64) -> f64 {
        self.num().unwrap_or(d)
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Data::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn str_or<'a>(&'a self, d: &'a str) -> &'a str {
        self.as_str().unwrap_or(d)
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Data::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_res(&self) -> Option<&Res> {
        match self {
            Data::Res(r) => Some(r),
            _ => None,
        }
    }
    /// Binary content: bytes, a string's UTF-8, or a resource holding `Vec<u8>`.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Data::Bytes(b) => Some(b),
            Data::Str(s) => Some(s.as_bytes()),
            Data::Res(r) => r.get::<Vec<u8>>().map(Vec::as_slice),
            _ => None,
        }
    }
    pub fn items(&self) -> &[Data] {
        match self {
            Data::List(l) => l,
            _ => &[],
        }
    }
    pub fn entries(&self) -> impl Iterator<Item = (&str, &Data)> {
        let m: Option<&BTreeMap<Arc<str>, Data>> = match self {
            Data::Map(m) => Some(m),
            _ => None,
        };
        m.into_iter().flat_map(|m| m.iter().map(|(k, v)| (&**k, v)))
    }
    /// Lua truthiness, plus `{ on = bool }` for toggle-shaped values.
    pub fn truthy(&self) -> bool {
        match self {
            Data::Nil => false,
            Data::Bool(b) => *b,
            Data::Map(m) => m.get("on").is_none_or(Data::truthy),
            _ => true,
        }
    }

    pub fn from_lua(v: &LuaValue) -> mlua::Result<Data> {
        from_lua(v, 0)
    }

    pub fn to_lua(&self, lua: &Lua) -> mlua::Result<LuaValue> {
        Ok(match self {
            Data::Nil => LuaValue::Nil,
            Data::Bool(b) => LuaValue::Boolean(*b),
            Data::Num(n) => LuaValue::Number(*n),
            Data::Str(s) => LuaValue::String(lua.create_string(s.as_bytes())?),
            Data::Bytes(b) => LuaValue::Buffer(lua.create_buffer(&b[..])?),
            Data::List(l) => {
                let t = lua.create_table_with_capacity(l.len(), 0)?;
                for (i, x) in l.iter().enumerate() {
                    t.raw_set(i + 1, x.to_lua(lua)?)?;
                }
                LuaValue::Table(t)
            }
            Data::Map(m) => {
                let t = lua.create_table_with_capacity(0, m.len())?;
                for (k, x) in m.iter() {
                    t.raw_set(&**k, x.to_lua(lua)?)?;
                }
                LuaValue::Table(t)
            }
            Data::Res(r) => LuaValue::UserData(lua.create_userdata(ResUd(r.clone()))?),
        })
    }

    /// JSON for agents and web APIs. Resources become `{"$res": "kind#id"}`,
    /// bytes `{"$bytes": length}`; whole numbers are written as integers.
    pub fn to_json(&self) -> Json {
        match self {
            Data::Nil => Json::Null,
            Data::Bool(b) => Json::Bool(*b),
            Data::Num(n) => {
                if n.fract() == 0.0 && n.abs() < 9.0e15 {
                    Json::from(*n as i64)
                } else {
                    serde_json::Number::from_f64(*n).map(Json::Number).unwrap_or(Json::Null)
                }
            }
            Data::Str(s) => Json::String(s.to_string()),
            Data::Bytes(b) => serde_json::json!({ "$bytes": b.len() }),
            Data::List(l) => Json::Array(l.iter().map(Data::to_json).collect()),
            Data::Map(m) => Json::Object(m.iter().map(|(k, v)| (k.to_string(), v.to_json())).collect()),
            Data::Res(r) => serde_json::json!({ "$res": format!("{}#{}", r.kind(), r.id()) }),
        }
    }

    pub fn from_json(j: &Json) -> Data {
        match j {
            Json::Null => Data::Nil,
            Json::Bool(b) => Data::Bool(*b),
            Json::Number(n) => Data::Num(n.as_f64().unwrap_or(0.0)),
            Json::String(s) => Data::str(s),
            Json::Array(a) => Data::list(a.iter().map(Data::from_json).collect()),
            Json::Object(o) => Data::Map(Arc::new(o.iter().map(|(k, v)| (Arc::from(k.as_str()), Data::from_json(v))).collect())),
        }
    }
}

fn err(msg: &str) -> mlua::Error {
    mlua::Error::runtime(msg)
}

fn key_string(k: &LuaValue) -> mlua::Result<Arc<str>> {
    match k {
        LuaValue::String(s) => Ok(Arc::from(&*s.to_str()?)),
        LuaValue::Integer(i) => Ok(Arc::from(i.to_string().as_str())),
        LuaValue::Number(n) => Ok(Arc::from(n.to_string().as_str())),
        _ => Err(err("table keys in values must be strings or numbers")),
    }
}

fn index_of(k: &LuaValue) -> Option<usize> {
    match k {
        LuaValue::Integer(i) if *i >= 1 => Some(*i as usize),
        LuaValue::Number(n) if *n >= 1.0 && n.fract() == 0.0 && *n < 9.0e15 => Some(*n as usize),
        _ => None,
    }
}

fn table_to_data(t: &Table, depth: u32) -> mlua::Result<Data> {
    let len = t.raw_len();
    let mut count = 0usize;
    let mut sequence = true;
    for pair in t.pairs::<LuaValue, LuaValue>() {
        let (k, _) = pair?;
        count += 1;
        if !index_of(&k).is_some_and(|i| i <= len) {
            sequence = false;
        }
    }
    if sequence && count == len {
        let mut v = Vec::with_capacity(len);
        for i in 1..=len {
            v.push(from_lua(&t.raw_get::<LuaValue>(i)?, depth + 1)?);
        }
        return Ok(Data::List(Arc::new(v)));
    }
    let mut m = BTreeMap::new();
    for pair in t.pairs::<LuaValue, LuaValue>() {
        let (k, x) = pair?;
        m.insert(key_string(&k)?, from_lua(&x, depth + 1)?);
    }
    Ok(Data::Map(Arc::new(m)))
}

fn from_lua(v: &LuaValue, depth: u32) -> mlua::Result<Data> {
    if depth > MAX_DEPTH {
        return Err(err("value nested too deeply (or contains a cycle)"));
    }
    Ok(match v {
        LuaValue::Nil => Data::Nil,
        LuaValue::Boolean(b) => Data::Bool(*b),
        LuaValue::Integer(i) => Data::Num(*i as f64),
        LuaValue::Number(n) => Data::Num(*n),
        LuaValue::String(s) => match s.to_str() {
            Ok(t) => Data::Str(Arc::from(&*t)),
            Err(_) => Data::Bytes(Arc::new(s.as_bytes().to_vec())),
        },
        LuaValue::Buffer(b) => Data::Bytes(Arc::new(b.to_vec())),
        LuaValue::Table(t) => table_to_data(t, depth)?,
        LuaValue::UserData(ud) => match ud.borrow::<ResUd>() {
            Ok(r) => Data::Res(r.0.clone()),
            Err(_) => return Err(err("a cell handle cannot be stored inside a value; pass it as an input")),
        },
        LuaValue::Function(_) => return Err(err("functions cannot be stored inside a value")),
        _ => return Err(err("unsupported value type")),
    })
}

/// What a script sees of one operator input. Values not ready yet arrive as
/// `{ state = "pending", progress }` or `{ state = "error", message }`.
pub fn input_to_lua(lua: &Lua, i: &Input) -> mlua::Result<LuaValue> {
    match &i.status {
        Status::Pending { progress } => {
            let t = lua.create_table_with_capacity(0, 2)?;
            t.raw_set("state", "pending")?;
            t.raw_set("progress", *progress as f64)?;
            Ok(LuaValue::Table(t))
        }
        Status::Error(e) => {
            let t = lua.create_table_with_capacity(0, 2)?;
            t.raw_set("state", "error")?;
            t.raw_set("message", e.as_str())?;
            Ok(LuaValue::Table(t))
        }
        Status::Empty => Ok(LuaValue::Nil),
        Status::Ready => match i.value.as_deref().map(value_to_data) {
            Some(d) => d.to_lua(lua),
            None => Ok(LuaValue::Nil),
        },
    }
}

/// A cell value as `Data` (script values, stream values, the clock, the camera level).
pub fn value_to_data(v: &(dyn Any + Send + Sync)) -> Data {
    if let Some(d) = v.downcast_ref::<Data>() {
        d.clone()
    } else if let Some(s) = v.downcast_ref::<Streamed>() {
        s.data.clone()
    } else if let Some(n) = v.downcast_ref::<f64>() {
        Data::Num(*n)
    } else if let Some(n) = v.downcast_ref::<i32>() {
        Data::Num(*n as f64)
    } else {
        Data::Nil
    }
}

/// An input as `Data` for native jobs: not-ready inputs as in [`input_to_lua`].
pub fn input_to_data(i: &Input) -> Data {
    match &i.status {
        Status::Pending { progress } => Data::map([("state", Data::str("pending")), ("progress", Data::Num(*progress as f64))]),
        Status::Error(e) => Data::map([("state", Data::str("error")), ("message", Data::str(e))]),
        Status::Empty => Data::Nil,
        Status::Ready => i.value.as_deref().map(value_to_data).unwrap_or_default(),
    }
}

/// Production time of the newest stream item among `inputs`.
pub fn newest_of(inputs: &[Input]) -> Option<Instant> {
    inputs.iter().filter_map(|i| i.get::<Streamed>().and_then(|s| s.newest)).max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_through_luau() {
        let lua = Lua::new();
        let d = Data::map([
            ("n", Data::Num(2.5)),
            ("s", Data::str("字")),
            ("l", Data::nums([1.0, 2.0, 3.0])),
            ("m", Data::map([("on", Data::Bool(true))])),
            ("b", Data::bytes(vec![0, 255, 7])),
        ]);
        let v = d.to_lua(&lua).unwrap();
        let back = Data::from_lua(&v).unwrap();
        assert_eq!(d, back);
        assert!(back.get("m").truthy());
        assert_eq!(back.get("l").items().len(), 3);
        assert_eq!(back.get("b").as_bytes(), Some(&[0u8, 255, 7][..]));
    }

    #[test]
    fn sequences_and_maps() {
        let lua = Lua::new();
        let v: LuaValue = lua.load("return { 1, 2, x = 3 }").eval().unwrap();
        assert!(matches!(Data::from_lua(&v).unwrap(), Data::Map(_)));
        let v: LuaValue = lua.load("return { 'a', 'b' }").eval().unwrap();
        assert_eq!(Data::from_lua(&v).unwrap(), Data::list(vec![Data::str("a"), Data::str("b")]));
        let v: LuaValue = lua.load("local t = {} t.self = t return t").eval().unwrap();
        assert!(Data::from_lua(&v).is_err());
    }

    #[test]
    fn resources_keep_identity_and_versions() {
        let lua = Lua::new();
        let r = Res::new("test.thing", 41u32);
        let v = Data::Res(r.clone()).to_lua(&lua).unwrap();
        let back = Data::from_lua(&v).unwrap();
        assert_eq!(back.as_res().unwrap().get::<u32>(), Some(&41));
        assert_eq!(back, Data::Res(r.clone()));
        assert_ne!(Data::Res(r.next_version()), Data::Res(r.clone()));
        lua.globals().set("x", v).unwrap();
        let kind: String = lua.load("return x.kind").eval().unwrap();
        assert_eq!(kind, "test.thing");
    }

    #[test]
    fn json_round_trip() {
        let j: Json = serde_json::from_str(r#"{"a":[1,2.5,"x",null,true],"b":{"c":3}}"#).unwrap();
        let d = Data::from_json(&j);
        assert_eq!(d.get("b").get("c").num(), Some(3.0));
        assert_eq!(d.to_json(), j);
    }
}
