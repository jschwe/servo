/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::env;
use std::path::PathBuf;
use std::sync::OnceLock;

use euclid::default::Size2D as UntypedSize2D;
use libloading::Library;
use log::{info, warn};
use paint_api::{CrossProcessPaintApi, WebRenderExternalImageApi, WebRenderExternalImageIdManager};
use servo_config::pref;
use webgpu_traits::{
    GenericReceiver, WEBGPU_PLUGIN_API_VERSION, WebGPU, WebGPUMsg, WebGpuPlugin,
    WebGpuPluginMetadata, WebGpuThreadConfig,
};

type PluginEntry = unsafe fn() -> Box<dyn WebGpuPlugin>;

struct LoadedPlugin {
    _library: Library,
    path: PathBuf,
    plugin: Box<dyn WebGpuPlugin>,
}

static LOADED_PLUGIN: OnceLock<Result<LoadedPlugin, String>> = OnceLock::new();

fn plugin_filename() -> &'static str {
    if cfg!(target_os = "windows") {
        "servo_webgpu_plugin.dll"
    } else if cfg!(target_os = "macos") {
        "libservo_webgpu_plugin.dylib"
    } else {
        "libservo_webgpu_plugin.so"
    }
}

fn plugin_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Ok(path) = env::var("SERVO_WEBGPU_PLUGIN") {
        paths.push(PathBuf::from(path));
    }

    if let Ok(mut path) = env::current_exe() {
        path.pop();
        paths.push(path.join(plugin_filename()));

        if cfg!(target_os = "macos") {
            let mut lib_dir = path;
            lib_dir.push("lib");
            paths.push(lib_dir.join(plugin_filename()));
        }
    }

    paths
}

fn load_plugin() -> Result<&'static LoadedPlugin, &'static str> {
    LOADED_PLUGIN
        .get_or_init(|| unsafe {
            let paths = plugin_paths();
            let mut errors = Vec::new();

            if paths.is_empty() {
                warn!("No WebGPU plugin candidate paths were found");
            } else {
                debug!("Trying WebGPU plugin candidate paths: {:?}", paths);
            }

            for path in paths {
                match Library::new(&path) {
                    Ok(library) => {
                        info!("Loaded WebGPU plugin library from {}", path.display());
                        let entry =
                            match library.get::<PluginEntry>(b"servo_webgpu_plugin_entry_v1") {
                                Ok(entry) => entry,
                                Err(error) => {
                                    errors.push(format!(
                                        "{}: failed to resolve entry symbol: {error}",
                                        path.display()
                                    ));
                                    continue;
                                },
                            };

                        let plugin = entry();
                        let metadata = plugin.metadata();
                        info!(
                            "WebGPU plugin metadata from {}: api_version={}, package_version={}",
                            path.display(),
                            metadata.api_version,
                            metadata.package_version
                        );
                        validate_metadata(metadata)
                            .map_err(|error| format!("{}: {error}", path.display()))?;

                        info!(
                            "WebGPU plugin validated successfully from {}",
                            path.display()
                        );
                        return Ok(LoadedPlugin {
                            _library: library,
                            path,
                            plugin,
                        });
                    },
                    Err(error) => errors.push(format!("{}: {error}", path.display())),
                }
            }

            Err(if errors.is_empty() {
                String::from("no candidate plugin paths were found")
            } else {
                errors.join("; ")
            })
        })
        .as_ref()
        .map_err(|error| error.as_str())
}

fn validate_metadata(metadata: WebGpuPluginMetadata) -> Result<(), String> {
    if metadata.api_version != WEBGPU_PLUGIN_API_VERSION {
        return Err(format!(
            "plugin API version mismatch: expected {}, got {}",
            WEBGPU_PLUGIN_API_VERSION, metadata.api_version
        ));
    }

    if metadata.package_version != env!("CARGO_PKG_VERSION") {
        return Err(format!(
            "plugin package version mismatch: expected {}, got {}",
            env!("CARGO_PKG_VERSION"),
            metadata.package_version
        ));
    }

    Ok(())
}

struct LazyWebGpuExternalImages {
    delegate: Option<Box<dyn WebRenderExternalImageApi>>,
}

impl LazyWebGpuExternalImages {
    fn delegate(&mut self) -> &mut dyn WebRenderExternalImageApi {
        if self.delegate.is_none() {
            let plugin = load_plugin().unwrap_or_else(|error| {
                panic!("WebGPU external image handler requested but plugin failed to load: {error}")
            });
            info!(
                "Creating WebGPU external image handler via plugin {}",
                plugin.path.display()
            );
            self.delegate = Some(plugin.plugin.create_external_image_handler());
        }

        self.delegate
            .as_deref_mut()
            .expect("delegate should be initialized")
    }
}

impl WebRenderExternalImageApi for LazyWebGpuExternalImages {
    fn lock(&mut self, id: u64) -> (paint_api::ExternalImageSource<'_>, UntypedSize2D<i32>) {
        self.delegate().lock(id)
    }

    fn unlock(&mut self, id: u64) {
        self.delegate().unlock(id)
    }
}

pub fn external_image_handler() -> Box<dyn WebRenderExternalImageApi> {
    Box::new(LazyWebGpuExternalImages { delegate: None })
}

pub fn start_webgpu_thread(
    paint_api: CrossProcessPaintApi,
    webrender_external_image_id_manager: WebRenderExternalImageIdManager,
) -> Option<(WebGPU, GenericReceiver<WebGPUMsg>)> {
    if !pref!(dom_webgpu_enabled) {
        return None;
    }

    match load_plugin() {
        Ok(plugin) => {
            info!(
                "Starting WebGPU thread via plugin {}",
                plugin.path.display()
            );
            let result = plugin.plugin.start_webgpu_thread(
                paint_api,
                webrender_external_image_id_manager,
                WebGpuThreadConfig {
                    wgpu_backend: pref!(dom_webgpu_wgpu_backend),
                },
            );
            if result.is_none() {
                warn!(
                    "WebGPU plugin {} returned no WebGPU thread",
                    plugin.path.display()
                );
            }
            result
        },
        Err(error) => {
            warn!("Failed to load the WebGPU plugin: {error}");
            None
        },
    }
}
