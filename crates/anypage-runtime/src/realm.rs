//! One page's Luau realm: its own VM, memory quota, per-call time budget and
//! frozen globals. Source text only; bytecode is never accepted.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mlua::chunk::ChunkMode;
use mlua::{Function, Lua, RegistryKey, Value as LuaValue, VmState};

use crate::package::Limits;

const PRELUDE: &str = include_str!("prelude.luau");
/// Globals a page has no business with: loading code at run time, or
/// switching environments (which would also turn off the sandbox's fast paths).
const REMOVED_GLOBALS: [&str; 7] = ["loadstring", "load", "dofile", "loadfile", "require", "getfenv", "setfenv"];
/// The interrupt fires on calls and loop back-edges; read the clock every N of them.
const CLOCK_EVERY: u32 = 128;

pub struct Realm {
    lua: Mutex<Lua>,
    /// Nanoseconds since `epoch` after which the running call is stopped; 0 = none.
    deadline: Arc<AtomicU64>,
    epoch: Instant,
    draw_new: RegistryKey,
    pub limits: Limits,
}

/// Push the running call's deadline back by `by`: time spent in native code
/// (text shaping, a PDF edit) is bounded by the module's own limits and does
/// not count against the script's budget.
pub fn pause_deadline(deadline: &AtomicU64, by: Duration) {
    let n = by.as_nanos().min(u64::MAX as u128) as u64;
    let _ = deadline.try_update(Ordering::Relaxed, Ordering::Relaxed, |v| if v == 0 { None } else { Some(v.saturating_add(n)) });
}

/// First lines of an mlua error, without the "runtime error:" prefix.
pub fn describe(e: &mlua::Error) -> String {
    let s = e.to_string();
    let s = s.strip_prefix("runtime error: ").unwrap_or(&s);
    let mut out: String = s
        .lines()
        .take_while(|l| !l.trim_start().starts_with("stack traceback"))
        .filter(|l| !l.trim().is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" | ");
    if let Some((i, _)) = out.char_indices().nth(400) {
        out.truncate(i);
        out.push('…');
    }
    out
}

impl Realm {
    pub fn new(limits: &Limits) -> Result<Self, String> {
        let lua = Lua::new();
        let deadline = Arc::new(AtomicU64::new(0));
        let epoch = Instant::now();
        {
            let deadline = deadline.clone();
            let count = AtomicU32::new(0);
            lua.set_interrupt(move |_| {
                let d = deadline.load(Ordering::Relaxed);
                if d != 0 && count.fetch_add(1, Ordering::Relaxed).is_multiple_of(CLOCK_EVERY) && epoch.elapsed().as_nanos() as u64 > d {
                    return Err(mlua::Error::runtime("time budget exceeded"));
                }
                Ok(VmState::Continue)
            });
        }
        lua.set_memory_limit((limits.memory_mb as usize) << 20).map_err(|e| describe(&e))?;
        let g = lua.globals();
        for name in REMOVED_GLOBALS {
            g.raw_set(name, LuaValue::Nil).map_err(|e| describe(&e))?;
        }
        lua.load(PRELUDE).set_name("prelude").set_mode(ChunkMode::Text).exec().map_err(|e| format!("prelude: {}", describe(&e)))?;
        let draw_new = (|| -> mlua::Result<RegistryKey> {
            let draw: mlua::Table = lua.globals().get("Draw")?;
            let f: Function = draw.get("new")?;
            lua.create_registry_value(f)
        })()
        .map_err(|e| describe(&e))?;
        Ok(Self { lua: Mutex::new(lua), deadline, epoch, draw_new, limits: *limits })
    }

    /// Install host functions before the page runs.
    pub fn setup(&self, f: impl FnOnce(&Lua) -> mlua::Result<()>) -> Result<(), String> {
        let lua = self.lua.lock().unwrap_or_else(|p| p.into_inner());
        f(&lua).map_err(|e| describe(&e))
    }

    /// Freeze every global and library table. Anything the page defines from
    /// now on lives in its own environment.
    pub fn seal(&self) -> Result<(), String> {
        let lua = self.lua.lock().unwrap_or_else(|p| p.into_inner());
        lua.sandbox(true).map_err(|e| describe(&e))
    }

    /// Run `f` with the VM, stopping any Luau code in it after `budget_ms`.
    pub fn call<R>(&self, budget_ms: f64, f: impl FnOnce(&Lua) -> mlua::Result<R>) -> Result<R, String> {
        let lua = self.lua.lock().unwrap_or_else(|p| p.into_inner());
        let until = self.epoch.elapsed() + Duration::from_secs_f64(budget_ms.max(0.1) / 1000.0);
        let prev = self.deadline.swap(until.as_nanos() as u64, Ordering::Relaxed);
        let r = f(&lua);
        self.deadline.store(prev, Ordering::Relaxed);
        r.map_err(|e| describe(&e))
    }

    /// Run a source chunk (never bytecode).
    pub fn exec(&self, name: &str, src: &str, budget_ms: f64) -> Result<(), String> {
        self.call(budget_ms, |lua| lua.load(src).set_name(format!("@{name}")).set_mode(ChunkMode::Text).exec())
    }

    pub fn draw_new(&self, lua: &Lua) -> mlua::Result<Function> {
        lua.registry_value(&self.draw_new)
    }

    /// The running call's deadline, so host functions can stop the clock
    /// while native code runs (see [`pause_deadline`]).
    pub fn deadline_handle(&self) -> Arc<AtomicU64> {
        self.deadline.clone()
    }

    pub fn used_memory(&self) -> usize {
        self.lua.lock().unwrap_or_else(|p| p.into_inner()).used_memory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn realm() -> Realm {
        let r = Realm::new(&Limits { memory_mb: 16, ..Limits::default() }).unwrap();
        r.seal().unwrap();
        r
    }

    #[test]
    fn budget_stops_runaway_code() {
        let r = realm();
        let t0 = Instant::now();
        let e = r.exec("loop", "while true do end", 20.0).unwrap_err();
        assert!(e.contains("time budget"), "{e}");
        assert!(t0.elapsed() < Duration::from_millis(500));
        // the realm is still usable afterwards
        r.exec("ok", "local x = 1 + 1", 20.0).unwrap();
    }

    #[test]
    fn memory_quota_and_frozen_globals() {
        let r = realm();
        assert!(r.exec("oom", "local t = {} for i = 1, 1e8 do t[i] = i end", 5000.0).is_err());
        assert!(r.exec("frozen", "color.rgba = nil", 50.0).is_err());
        assert!(r.exec("frozen2", "math.floor = nil", 50.0).is_err());
        assert!(r.exec("removed", "loadstring('return 1')", 50.0).is_err());
        assert!(r.exec("bytecode", "\x1bLua", 50.0).is_err());
        r.exec("own", "Mine = 1", 50.0).unwrap();
    }

    #[test]
    fn prelude_colours() {
        let r = realm();
        let v: f64 = r.call(50.0, |lua| lua.load("return color.rgba(17, 34, 51, 68)").eval()).unwrap();
        assert_eq!(v as u32, 0x11223344);
        let v: f64 = r.call(50.0, |lua| lua.load("return color.alpha(color.rgba(1, 2, 3), 9)").eval()).unwrap();
        assert_eq!(v as u32, 0x01020309);
    }
}
