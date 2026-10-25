//! Operators the runtime puts into a page's graph: Luau functions, native
//! `Data` jobs, and adapters between script values and engine values.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use anypage_core::{val, EvalCtx, Exec, Input, Op, OpClass, Value};
use mlua::{Function, MultiValue, RegistryKey, Table, Value as LuaValue};

use crate::data::{input_to_data, input_to_lua, newest_of, Data};
use crate::draw::DisplayList;
use crate::natives::{Grants, JobCtx, JobFnData};
use crate::realm::Realm;

/// Where script code is running, for [`crate::natives::Effect`] checks.
pub const PHASE_LOAD: u8 = 0;
pub const PHASE_HANDLER: u8 = 1;
pub const PHASE_EVAL: u8 = 2;

pub(crate) struct PhaseGuard<'a> {
    phase: &'a AtomicU8,
    prev: u8,
}

impl<'a> PhaseGuard<'a> {
    pub fn enter(phase: &'a AtomicU8, p: u8) -> Self {
        let prev = phase.swap(p, Ordering::Relaxed);
        Self { phase, prev }
    }
}

impl Drop for PhaseGuard<'_> {
    fn drop(&mut self) {
        self.phase.store(self.prev, Ordering::Relaxed);
    }
}

pub(crate) enum ScriptKind {
    Derive,
    Draw { w: f32, h: f32 },
}

fn until_of(v: Option<&LuaValue>) -> Option<f64> {
    match v {
        Some(LuaValue::Number(n)) if !n.is_nan() => Some(*n),
        Some(LuaValue::Integer(i)) => Some(*i as f64),
        _ => None,
    }
}

/// An operator whose function is Luau. Runs inline on the evaluating thread
/// under the realm's time budget; its result is compared for early cutoff.
pub(crate) struct ScriptOp {
    pub name: String,
    pub realm: Arc<Realm>,
    pub func: RegistryKey,
    pub kind: ScriptKind,
    pub phase: Arc<AtomicU8>,
    pub last_error: Mutex<Option<String>>,
}

impl Op for ScriptOp {
    fn name(&self) -> &str {
        &self.name
    }
    fn class(&self) -> OpClass {
        OpClass::Script
    }
    fn eval(&self, inputs: &[Input], ctx: &EvalCtx) -> Result<Value, String> {
        let realm = &self.realm;
        let _phase = PhaseGuard::enter(&self.phase, PHASE_EVAL);
        let r = realm.call(realm.limits.call_ms, |lua| {
            let f: Function = lua.registry_value(&self.func)?;
            let mut args: Vec<LuaValue> = Vec::with_capacity(inputs.len() + 1);
            let builder = match self.kind {
                ScriptKind::Draw { w, h } => {
                    let t: Table = realm.draw_new(lua)?.call((w, h))?;
                    args.push(LuaValue::Table(t.clone()));
                    Some(t)
                }
                ScriptKind::Derive => None,
            };
            for i in inputs {
                args.push(input_to_lua(lua, i)?);
            }
            let rets: Vec<LuaValue> = f.call::<MultiValue>(args.into_iter().collect::<MultiValue>())?.into_iter().collect();
            match builder {
                Some(t) => Ok((Data::from_lua(&t.raw_get::<LuaValue>("cmds")?)?, until_of(rets.first()))),
                None => Ok((Data::from_lua(rets.first().unwrap_or(&LuaValue::Nil))?, until_of(rets.get(1)))),
            }
        });
        let mut last = self.last_error.lock().unwrap_or_else(|p| p.into_inner());
        match r {
            Ok((data, until)) => {
                if let Some(t) = until {
                    ctx.valid_until(t);
                }
                *last = None;
                Ok(match self.kind {
                    ScriptKind::Draw { .. } => val(DisplayList { cmds: data, newest: newest_of(inputs) }),
                    ScriptKind::Derive => val(data),
                })
            }
            Err(e) => {
                if last.as_deref() != Some(e.as_str()) {
                    log::warn!(target: "anypage_page", "{}: {e}", self.name);
                    *last = Some(e.clone());
                }
                Err(e)
            }
        }
    }
    fn same(&self, a: &Value, b: &Value) -> bool {
        match self.kind {
            ScriptKind::Draw { .. } => match (a.downcast_ref::<DisplayList>(), b.downcast_ref::<DisplayList>()) {
                (Some(x), Some(y)) => x == y,
                _ => false,
            },
            ScriptKind::Derive => match (a.downcast_ref::<Data>(), b.downcast_ref::<Data>()) {
                (Some(x), Some(y)) => x == y,
                _ => false,
            },
        }
    }
}

/// A native job registered with [`crate::Natives::job_fn`], on the worker pool.
pub(crate) struct DataJob {
    pub name: String,
    pub f: JobFnData,
    pub opts: Data,
    pub grants: Arc<Grants>,
}

impl Op for DataJob {
    fn name(&self) -> &str {
        &self.name
    }
    fn exec(&self) -> Exec {
        Exec::Async
    }
    fn eval(&self, inputs: &[Input], ctx: &EvalCtx) -> Result<Value, String> {
        let data: Vec<Data> = inputs.iter().map(input_to_data).collect();
        let jc = JobCtx { inputs: &data, opts: &self.opts, grants: &self.grants, eval: ctx };
        crate::natives::guarded(&self.name, || (self.f)(&jc)).map(val)
    }
    fn same(&self, a: &Value, b: &Value) -> bool {
        matches!((a.downcast_ref::<Data>(), b.downcast_ref::<Data>()), (Some(x), Some(y)) if x == y)
    }
}
