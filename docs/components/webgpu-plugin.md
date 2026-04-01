# Load-On-Demand WebGPU Plugin Design

This note describes the smallest viable design for moving Servo's WebGPU
implementation into a Rust dynamic library that is loaded on first use.

Assumptions for this design:

- The host and plugin are built from the same source tree.
- The host and plugin use the same Rust compiler version and target.
- The plugin is loaded once and never unloaded.
- The goal is reducing the default Servo/servoshell binary size, not creating a
  stable third-party plugin API.

## Goal

Make the default Servo build not link `wgpu-core` or the `components/webgpu`
implementation, while preserving WebGPU support when a matching plugin library
is present.

## Why the current boundary is not enough

Servo already starts WebGPU lazily at runtime:

- `constellation` only creates a WebGPU thread on first use.
- `components/webgpu` exposes `start_webgpu_thread(...)`.

That is not sufficient for binary size, because the host still links the
implementation and its heavy dependencies today.

The main blocker is that [`components/shared/webgpu/lib.rs`](/Users/jschwender/Dev/servo/components/shared/webgpu/lib.rs)
mixes two roles:

- wire/protocol types that the DOM, script, constellation, and paint code need
- implementation-facing types that directly depend on `wgpu-core`

Examples:

- `HostMap` from `wgpu_core::device`
- `ComputePassId`, `RenderPassId`, `DeviceId`, `QueueId`
- `AdapterInfo`, `Features`, `Limits`

As long as those types live in the host-facing shared crate, the main binary
still needs `wgpu-core`, even if the actual thread startup moves into a dylib.

## Proposed split

Split the current shared WebGPU crate into two layers.

### 1. `servo-webgpu-protocol`

New crate containing only host/plugin protocol types.

Requirements:

- no `wgpu-core` dependency
- no direct `wgpu-types` dependency unless it is proven small enough and worth
  keeping
- serializable or trivially clonable message types
- opaque identifiers represented as Servo-owned newtypes, not `wgpu-core` IDs

This crate becomes the only WebGPU crate linked into:

- `script`
- `shared/script`
- `shared/constellation`
- `constellation`
- `paint`
- `servo`

### 2. `servo-webgpu-runtime`

This is the current `components/webgpu` implementation plus any remaining
`wgpu-core`-specific shared code.

Responsibilities:

- own the `wgpu-core` dependency
- implement adapter/device/pipeline/resource operations
- run the WebGPU worker thread
- translate protocol IDs into runtime-local `wgpu-core` IDs
- manage staging buffers and canvas presentation

The key change is that `wgpu-core` IDs stop crossing the host/plugin boundary.

## Same-toolchain Rust plugin ABI

Given the constraints for this project, the smallest design is a same-toolchain
Rust ABI, not a C ABI.

Add a small API crate, for example `servo-webgpu-plugin-api`, that defines the
interface shared by the host and the plugin.

Suggested shape:

```rust
pub trait WebGpuPluginFactory: Send + Sync {
    fn start(
        &self,
        startup: WebGpuStartup,
    ) -> Result<WebGpuPluginInstance, WebGpuPluginError>;
}

pub struct WebGpuStartup {
    pub paint_api: paint_api::CrossProcessPaintApi,
    pub external_image_ids: paint_api::WebRenderExternalImageIdManager,
    pub image_map: Arc<Mutex<FxHashMap<WebGPUContextId, ContextData>>>,
}

pub struct WebGpuPluginInstance {
    pub channel: webgpu_protocol::WebGPU,
    pub receiver: servo_base::generic_channel::GenericReceiver<webgpu_protocol::WebGPUMsg>,
}
```

Plugin export:

```rust
#[no_mangle]
pub fn servo_webgpu_plugin_entry_v1() -> Box<dyn WebGpuPluginFactory>
```

Host loading:

- use `libloading`
- load a single well-known symbol
- keep the loaded `Library` alive for process lifetime
- cache the factory in a singleton

This is intentionally not a stable ABI. It is only valid when the host and
plugin are built together.

## Why this is acceptable here

This approach would normally be a bad public plugin ABI, but it is acceptable
for Servo's own deployment model because:

- the plugin ships with the exact matching host build
- there is no unload requirement
- there is no third-party compatibility requirement
- it minimizes engineering work compared to a full C ABI

## Host-side changes

### New loader crate

Add a small host crate, for example `servo-webgpu-loader`, that is always linked
but tiny. It owns:

- locating the plugin library
- `libloading::Library`
- symbol lookup
- version/hash checks
- singleton caching

Pseudo-API:

```rust
pub fn start_webgpu(
    startup: WebGpuStartup,
) -> Result<WebGpuPluginInstance, WebGpuPluginError>;
```

### Constellation

Replace the direct dependency on `components/webgpu` with the loader.

Today:

```rust
use webgpu::start_webgpu_thread;
```

Proposed:

```rust
use servo_webgpu_loader::start_webgpu;
```

The rest of `handle_wgpu_request` can stay close to its current structure.

### Preference behavior

Keep the existing `dom_webgpu_enabled` pref.

Behavior:

- if pref is false: return `None` as today
- if pref is true but plugin is missing: report a clear error once and act as if
  WebGPU is unavailable
- if pref is true and plugin loads: start lazily on first request

## Plugin crate

Add a new crate, for example `components/webgpu_plugin`.

Cargo settings:

- `crate-type = ["cdylib"]`

Dependencies:

- `webgpu`
- `servo-webgpu-plugin-api`
- `servo-webgpu-protocol`

Implementation:

- export `servo_webgpu_plugin_entry_v1`
- internally just forward to the current `webgpu::start_webgpu_thread(...)`

This keeps almost all existing runtime logic in place.

## Required protocol cleanup

This is the most important refactor.

The protocol crate must stop exposing these categories across the host/plugin
boundary:

- `wgpu_core::*Id`
- `wgpu_core::device::HostMap`
- `wgpu_core` error types
- runtime-only resource structs tied to `wgpu-core`

Replace them with Servo-owned protocol types, for example:

```rust
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct WebGpuDeviceId(pub u64);

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum MapMode {
    Read,
    Write,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AdapterInfo {
    pub name: String,
    pub vendor: u32,
    pub device: u32,
    pub backend: BackendKind,
    pub device_type: DeviceType,
}
```

The plugin then owns the translation layer:

- protocol ID <-> runtime `wgpu-core` ID
- protocol enums <-> `wgpu-types`

## External image integration

The current WebGPU canvas path is already a good in-process seam:

- host provides paint/external-image managers
- WebGPU runtime updates an external-image map
- WebRender locks images through the existing handler path

Because the plugin is in-process, this part can stay almost unchanged in the
first version. The startup payload simply passes the existing paint-side handles
into the plugin.

That is one reason a same-toolchain in-process plugin is much smaller than a
full out-of-process GPU service.

## Failure model

The loader should distinguish:

- plugin library not found
- symbol not found
- version mismatch
- startup failure

Suggested host behavior:

- log a single structured error
- mark WebGPU unavailable for the process
- continue browser startup normally

The browser should not crash merely because the optional WebGPU plugin is absent.

## Versioning

Even with same-toolchain builds, add a simple version handshake.

At minimum:

- exported symbol name includes `v1`
- the factory reports a build hash or Cargo package version
- the host compares it with its own expected value before using the plugin

Example:

```rust
pub trait WebGpuPluginFactory {
    fn metadata(&self) -> WebGpuPluginMetadata;
    fn start(&self, startup: WebGpuStartup) -> Result<WebGpuPluginInstance, WebGpuPluginError>;
}
```

## Recommended rollout

### Phase 1

Split `shared/webgpu` into:

- protocol crate with no `wgpu-core`
- runtime-facing crate with `wgpu-core`

This phase is mandatory to get real binary-size wins.

### Phase 2

Introduce `servo-webgpu-loader` and a statically linked fallback implementation.

This allows testing the loader shape before introducing dynamic linking.

### Phase 3

Add `components/webgpu_plugin` as a `cdylib`.

Default `servoshell` behavior:

- no static `webgpu` feature
- optional runtime load from plugin path

### Phase 4

Add packaging support for:

- macOS: `.dylib`
- Linux: `.so`
- Windows: `.dll`

and a predictable search path, for example next to the executable or under
`lib/`.

## Why not jump straight to a C ABI

A C ABI would be more stable, but it would force a much larger redesign now:

- opaque handles everywhere
- byte-oriented messaging everywhere
- no direct use of Servo channel types across the boundary
- likely more heap allocation and translation code

For a Servo-owned optional runtime shipped with the same build, the Rust ABI
design is much cheaper and still meets the stated constraints.

## Recommendation

If the goal is "optional, lazy-loaded WebGPU with smaller default binaries",
this is the recommended path:

1. split the protocol crate away from `wgpu-core`
2. add a small same-toolchain Rust plugin API
3. load a `cdylib` on first WebGPU use
4. never unload it

That keeps the change tractable and preserves almost all current runtime logic.
