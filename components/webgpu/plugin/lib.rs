/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use paint_api::{CrossProcessPaintApi, WebRenderExternalImageApi, WebRenderExternalImageIdManager};
use webgpu::canvas_context::WebGpuExternalImageMap;
use webgpu_traits::{
    GenericReceiver, WEBGPU_PLUGIN_API_VERSION, WebGPU, WebGPUMsg, WebGpuPlugin,
    WebGpuPluginMetadata, WebGpuThreadConfig,
};

#[derive(Default)]
struct ServoWebGpuPlugin {
    image_map: WebGpuExternalImageMap,
}

impl WebGpuPlugin for ServoWebGpuPlugin {
    fn metadata(&self) -> WebGpuPluginMetadata {
        WebGpuPluginMetadata {
            api_version: WEBGPU_PLUGIN_API_VERSION,
            package_version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn create_external_image_handler(&self) -> Box<dyn WebRenderExternalImageApi> {
        Box::new(webgpu::WebGpuExternalImages::new(self.image_map.clone()))
    }

    fn start_webgpu_thread(
        &self,
        paint_api: CrossProcessPaintApi,
        webrender_external_image_id_manager: WebRenderExternalImageIdManager,
        config: WebGpuThreadConfig,
    ) -> Option<(WebGPU, GenericReceiver<WebGPUMsg>)> {
        webgpu::start_webgpu_thread(
            paint_api,
            webrender_external_image_id_manager,
            self.image_map.clone(),
            config,
        )
    }
}

#[unsafe(no_mangle)]
pub fn servo_webgpu_plugin_entry_v1() -> Box<dyn WebGpuPlugin> {
    Box::new(ServoWebGpuPlugin::default())
}
