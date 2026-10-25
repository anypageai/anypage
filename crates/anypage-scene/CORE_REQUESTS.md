# Core requests from `anypage-scene`

Changes to the runtime or engine that would help the 3D module. Each one is
worked around today; none blocks the module.

## 1. Binary assets in page packages

**Why.** Packages accept only text files (`luau`, `wgsl`, `json`, `txt`, `md`),
so a 3D page cannot ship a `.glb`, `.ply` or `.las` sample. The scene demo
builds its models in code (`scene.primitive`, `scene.synthetic`, an OBJ in a
Luau string) instead.

**Proposal.** Allow binary assets with a per-file and per-package size limit,
read as a byte resource (kind `fs.bytes`, so every module accepts it):

```rust
// package.rs
pub struct Package { pub files: BTreeMap<String, String>, pub assets: BTreeMap<String, Arc<Vec<u8>>>, .. }
```
```lua
local bytes = page.asset("bunny.glb")   -- Res fs.bytes; nil if absent
```

## 2. Pixel scale in `GpuTarget`

**Why.** `point_size` and grid line widths are in target pixels. The engine
renders a GPU layer at its on-screen pixel size, so zooming the page changes
the target size but not the meaning of "pixels": points look the same size on
screen (which is the intended behaviour today), but a renderer cannot tell a
2x zoom from a 2x larger layer, e.g. to keep points a fixed size in world
units or to pick a cheaper LOD while zoomed out.

**Proposal.** `pub struct GpuTarget<'a> { .., pub scale: f32 /* pixels per world unit */ }`.

## 3. Progressive GPU layers (redraw request)

**Why.** `GpuRenderer::render` runs only when the layer value or size
changes. For clouds over the point budget the renderer draws a uniform prefix;
it cannot refine (draw more points over the next frames while the camera is
still) because nothing calls it again.

**Proposal.** Either a return value or a flag on the target:

```rust
pub enum RenderOutcome { Done, MoreNextFrame }
fn render(&self, target: &GpuTarget, value: &Value) -> Result<RenderOutcome, String>;
```

## 4. Device loss hook

**Why.** The renderer caches buffers and pipelines per device and notices a new
device by comparing `wgpu::Device` identity on the next call. The old
device's buffers stay alive until then.

**Proposal.** `trait GpuRenderer { fn device_lost(&self) {} }` called by the
engine when it recreates the device (default: nothing).

## 5. Adapter capabilities in `GpuTarget`

**Why.** 4x MSAA support for `target.format` is checked by creating a tiny
texture inside a validation error scope once per device, because only the
device (not the adapter) is reachable.

**Proposal.** `pub struct GpuTarget<'a> { .., pub msaa_samples: u32 /* largest supported count for format */ }`,
or the adapter's `TextureFormatFeatures` for `format`.

## 6. Storage buffers on capable devices (later)

**Why.** The engine requests `Limits::downlevel_webgl2_defaults()`, so no
storage buffers are available to vertex or compute shaders. Everything here
works within that (vertex and instance buffers), but GPU-side culling and
level-of-detail selection for 100M-point clouds would need compute shaders
with storage buffers.

**Proposal.** Request `downlevel_defaults()` (or the adapter's limits) when the
adapter supports them and expose the granted limits to renderers (they are
already readable through `device.limits()`).

## 7. Layer identity in `GpuTarget`

**Why.** One `SceneRenderer` serves every `scene.view` layer of every page,
and `render` is not told which page or layer it is drawing. Render stats are
therefore keyed by the scene table's `id` (a page-chosen string): two pages
that use the same `id`, or none, see each other's stats (timings and counts,
nothing else). Per-layer state (progressive refinement, per-layer caches)
has the same problem.

**Proposal.** `pub struct GpuTarget<'a> { .., pub layer: (u64 /* page instance */, u32 /* layer id */) }`,
and `SourceCtx` carrying the same page instance id, so a module can match a
page's stream to that page's layers.

## 8. Error scopes for out-of-memory in the engine

**Why.** The engine wraps `GpuRenderer::render` in a `Validation` error
scope only. An out-of-memory or internal error raised inside `render` (by
any renderer) reaches wgpu's default uncaptured-error handler, which panics.
`anypage-scene` now creates its buffers, textures, shaders and pipelines inside its own
out-of-memory / validation / internal scopes, but other renderers may not.

**Proposal.** Push `OutOfMemory` and `Internal` scopes around `render` as
well, and treat any error as a failed render.
