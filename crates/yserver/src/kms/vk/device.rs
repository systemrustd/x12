//! VkContext: instance + physical/logical device + queues + debug messenger.
//!
//! Lifetime is the full backend lifetime. Drop order matters:
//! device-level handles before device, device before instance,
//! instance-level loaders before instance.

use ash::vk;
use std::{
    ffi::{CStr, c_char, c_void},
    sync::Arc,
};

/// Lives for the entire backend lifetime. Drop order matters: device
/// before instance; instance-level loaders before instance.
///
/// Extension loaders (`debug_utils_instance`, `external_semaphore_fd`)
/// must be stored, not reconstructed per call: the underlying ash
/// loader resolves function pointers via `vkGetInstanceProcAddr` /
/// `vkGetDeviceProcAddr` once and caches them. Drop also goes through
/// the loader (`destroy_debug_utils_messenger`).
#[allow(dead_code)] // fields populated incrementally across sub-phase 4.1.1.
pub struct VkContext {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub debug_utils_instance: ash::ext::debug_utils::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,
    pub external_semaphore_fd: ash::khr::external_semaphore_fd::Device,
    pub external_memory_fd: Option<ash::khr::external_memory_fd::Device>,
    pub image_drm_format_modifier_ext: Option<ash::ext::image_drm_format_modifier::Device>,
    /// True when `VK_EXT_image_drm_format_modifier` is enabled on the
    /// device. Phase 4.2 DRI3 import needs this for non-LINEAR tilings;
    /// when false, `kms::vk::dri3::supported_modifiers` returns
    /// `[DRM_FORMAT_MOD_LINEAR]` per design §4 fallback matrix.
    pub image_drm_format_modifier: bool,
    /// GLX-TFP: per-driver tiling strategy for the exported image,
    /// cached on first successful allocation. LINEAR is preferred —
    /// Turnip / Adreno same-GPU dma-buf sharing only delivers live
    /// pixels through LINEAR (its modifier-tiled UBWC keeps
    /// compression metadata in driver caches that don't reach the
    /// dma-buf-backed memory, so the GL importer samples a frozen
    /// snapshot). RADV rejects LINEAR + COLOR_ATTACHMENT + dma-buf
    /// with `VK_ERROR_FORMAT_NOT_SUPPORTED`, in which case
    /// [`super::target::allocate_exportable`] falls back to the
    /// modifier path and caches that. Empty until the first
    /// allocation attempt.
    pub tfp_tiling_strategy: std::sync::OnceLock<super::target::TilingStrategy>,
    pub graphics_queue_family: u32,
    pub graphics_queue: vk::Queue,
    pub debug_messenger: Option<vk::DebugUtilsMessengerEXT>,
    /// Cached `VkPhysicalDeviceDriverProperties::driverID` for the
    /// picked device. Kept as a diagnostic for log lines / future
    /// driver-specific quirks; the scanout path itself no longer
    /// branches on it (the GBM-first cross-driver Venus problem
    /// went away with the Vulkan-first pivot).
    #[allow(dead_code)]
    pub driver_id: vk::DriverId,
    /// `VkPhysicalDeviceProperties::deviceType` of the picked device.
    /// `CPU` means a software rasterizer (llvmpipe/lavapipe) — usable
    /// for headless tests but NOT for real KMS scanout (see
    /// [`Self::is_software_rasterizer`]).
    pub device_type: vk::PhysicalDeviceType,
}

impl VkContext {
    /// Whether this driver should advertise DRI3/Present syncobj.
    ///
    /// The implementation currently imports DRI3 syncobj fds as
    /// timeline semaphores with `OPAQUE_FD`. That works on the
    /// Vulkan stacks we have used for Venus/Mesa testing, but NVIDIA
    /// proprietary rejects the very first import with
    /// `ERROR_INITIALIZATION_FAILED` ("Failed to allocate semaphore
    /// device memory"). Advertising only DRI3 1.3 on that driver lets
    /// clients fall back to the older fence-fd path instead of dying
    /// on `ImportSyncobj`.
    #[must_use]
    pub fn supports_dri3_syncobj(&self) -> bool {
        !matches!(self.driver_id, vk::DriverId::NVIDIA_PROPRIETARY)
    }

    pub fn new() -> Result<Arc<Self>, VkInitError> {
        let entry = unsafe { ash::Entry::load()? };
        let app_info = vk::ApplicationInfo::default()
            .application_name(c"yserver")
            .application_version(vk::make_api_version(0, 0, 1, 0))
            .engine_name(c"yserver-kms")
            .api_version(vk::API_VERSION_1_3);

        let ext_cstrs = super::instance::required_instance_extensions();
        let ext_ptrs: Vec<_> = ext_cstrs.iter().map(|c| c.as_ptr()).collect();

        // Validation layer in debug builds only, and only if the
        // installed Vulkan loader actually has it. Some environments
        // (e.g. the vng guest with no `vulkan-validation-layers`
        // installed) don't ship it; without this guard,
        // `vkCreateInstance` returns `VK_ERROR_LAYER_NOT_PRESENT` and
        // the whole backend falls back to pixman.
        //
        // Enable cases:
        //   - debug build: always try (validation-layer cost is fine).
        //   - release build with `YSERVER_VK_VALIDATION` set: opt-in
        //     for diagnosing release-mode-only bugs (e.g. perf-branch
        //     timeline-semaphore races) without rebuilding debug.
        let validation_layer_name = c"VK_LAYER_KHRONOS_validation";
        let validation_requested =
            cfg!(debug_assertions) || std::env::var_os("YSERVER_VK_VALIDATION").is_some();
        let validation_available =
            validation_requested && validation_layer_present(&entry, validation_layer_name);
        let layer_ptrs: Vec<*const c_char> = if validation_available {
            vec![validation_layer_name.as_ptr()]
        } else {
            Vec::new()
        };
        if validation_requested && !validation_available {
            log::warn!(
                "vulkan: validation layer requested but not present (install \
                 `vulkan-validation-layers` package); continuing without"
            );
        } else if validation_available {
            log::info!("vulkan: validation layer enabled");
        }

        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&ext_ptrs)
            .enabled_layer_names(&layer_ptrs);

        let instance = unsafe { entry.create_instance(&create_info, None)? };

        // Build the rest with manual error-cleanup. If any step after
        // create_instance fails, we must destroy the instance; same
        // applies to debug messenger / device once they exist.
        let debug_utils_instance = ash::ext::debug_utils::Instance::new(&entry, &instance);

        let debug_messenger = match create_debug_messenger(&debug_utils_instance) {
            Ok(m) => m,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(e);
            }
        };

        let (physical_device, graphics_queue_family) = match pick_physical_device(&instance) {
            Ok(t) => t,
            Err(e) => {
                unsafe {
                    if let Some(m) = debug_messenger {
                        debug_utils_instance.destroy_debug_utils_messenger(m, None);
                    }
                    instance.destroy_instance(None);
                }
                return Err(e);
            }
        };

        // Device extensions actually used by Phase 4.1.2's
        // Vulkan-first scanout path:
        //
        // - VK_KHR_external_memory_fd: vkGetMemoryFdKHR (export the
        //   bound image memory as a dma-buf).
        // - VK_EXT_external_memory_dma_buf: handle type `DMA_BUF` for
        //   the export (`ExternalMemoryImageCreateInfo` + the alloc).
        // - VK_KHR_external_semaphore_fd: vkGetSemaphoreFdKHR(SYNC_FD)
        //   for the IN_FENCE_FD handoff to KMS.
        //
        // Phase 4.2 reintroduction: VK_EXT_image_drm_format_modifier
        // is now requested for DRI3 tiled-image import. Drivers that
        // lack it (notably lavapipe at the time of writing) will still
        // function — `supported_modifiers` returns `[LINEAR]` per the
        // design §4 fallback matrix.
        //
        // Intentionally NOT requested:
        // - VK_KHR_swapchain — WSI is out of scope (design §1); KMS
        //   pageflip is our presentation path.
        // - VK_KHR_dynamic_rendering_local_read — only Phase 4.1.4.6
        //   ShaderRMW PictOps need this; deferred until that lands.
        //
        // The filter still drops anything the picked device doesn't
        // expose; on a healthy device every wanted extension makes
        // it through. The warning path remains as an early-fail signal
        // for misconfigured environments.
        let wanted: &[&CStr] = &[
            ash::khr::external_memory_fd::NAME,
            ash::ext::external_memory_dma_buf::NAME,
            ash::khr::external_semaphore_fd::NAME,
            ash::ext::image_drm_format_modifier::NAME,
        ];
        let supported_device_exts =
            match unsafe { instance.enumerate_device_extension_properties(physical_device) } {
                Ok(v) => v,
                Err(e) => {
                    unsafe {
                        if let Some(m) = debug_messenger {
                            debug_utils_instance.destroy_debug_utils_messenger(m, None);
                        }
                        instance.destroy_instance(None);
                    }
                    return Err(VkInitError::Vk(e));
                }
            };
        let device_extension_names: Vec<&'static CStr> = wanted
            .iter()
            .copied()
            .filter(|ext| {
                let ok = supported_device_exts.iter().any(|p| {
                    p.extension_name_as_c_str()
                        .map(|s| s == *ext)
                        .unwrap_or(false)
                });
                if !ok {
                    log::warn!(
                        "vulkan: physical device lacks {} — Vulkan-fed scanout will not work",
                        ext.to_string_lossy()
                    );
                }
                ok
            })
            .collect();
        let device_extensions: Vec<*const c_char> =
            device_extension_names.iter().map(|c| c.as_ptr()).collect();

        let priorities = [1.0_f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(graphics_queue_family)
            .queue_priorities(&priorities)];

        let mut features13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(true)
            .synchronization2(true);

        // `scalarBlockLayout` lets push-constant (and uniform/storage)
        // blocks use scalar packing rather than std140/std430's
        // 16-byte vec4 alignment, so a `vec4` after a stretch of
        // `vec2` fields lands directly after them — matching what
        // a `#[repr(C)]` Rust struct produces with no padding. This
        // sidesteps the alignment-mismatch bug class that produced
        // green text in `TextPushConsts` (vec4 expected at offset
        // 48 by std430, sat at offset 40 in Rust). The shaders that
        // rely on this declare `layout(scalar)` on the
        // `push_constant` block; the legacy `LogicFillPushConsts`
        // pad and the natural alignment of `RenderPushConsts` /
        // `CompositePushConsts` keep std430 layout intact and stay
        // compatible.
        // `timelineSemaphore` is core in Vulkan 1.2 and is required by
        // Phase 4.2.2's `import_drm_syncobj` (DRI3 ImportSyncobj path).
        // Harmless when the syncobj cap is false because the dispatcher
        // gate rejects requests before they reach the import call.
        let mut features12 = vk::PhysicalDeviceVulkan12Features::default()
            .scalar_block_layout(true)
            .timeline_semaphore(true);

        // `logicOp` enables the per-attachment logical-op state used
        // by the Phase 4.1.5 GC-function fill path (Xor / And / Or
        // / Invert / etc. — all 16 X11 GcFunction variants map 1:1
        // to `VkLogicOp`). `dualSrcBlend` enables the SRC1_* family
        // of blend factors used by the RENDER `component_alpha`
        // path (per-channel src alpha emitted from a second
        // fragment-shader output). Both are core Vulkan 1.0 and
        // universally supported on conformant drivers.
        let enabled_features = vk::PhysicalDeviceFeatures::default()
            .logic_op(true)
            .dual_src_blend(true);

        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_info)
            .enabled_extension_names(&device_extensions)
            .enabled_features(&enabled_features)
            .push_next(&mut features12)
            .push_next(&mut features13);

        let device = match unsafe { instance.create_device(physical_device, &device_info, None) } {
            Ok(d) => d,
            Err(e) => {
                unsafe {
                    if let Some(m) = debug_messenger {
                        debug_utils_instance.destroy_debug_utils_messenger(m, None);
                    }
                    instance.destroy_instance(None);
                }
                return Err(VkInitError::Vk(e));
            }
        };
        let graphics_queue = unsafe { device.get_device_queue(graphics_queue_family, 0) };
        let external_semaphore_fd =
            ash::khr::external_semaphore_fd::Device::new(&instance, &device);
        let external_memory_fd_supported =
            device_extension_names.contains(&ash::khr::external_memory_fd::NAME);
        let external_memory_fd = if external_memory_fd_supported {
            Some(ash::khr::external_memory_fd::Device::new(
                &instance, &device,
            ))
        } else {
            None
        };
        let image_drm_format_modifier =
            device_extension_names.contains(&ash::ext::image_drm_format_modifier::NAME);
        let image_drm_format_modifier_ext = if image_drm_format_modifier {
            Some(ash::ext::image_drm_format_modifier::Device::new(
                &instance, &device,
            ))
        } else {
            None
        };

        // Driver-id query. Diagnostic-only after the Vulkan-first
        // pivot — no path branches on it. Kept so future quirks can
        // re-introduce branches without re-querying.
        let mut driver_props = vk::PhysicalDeviceDriverProperties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver_props);
        unsafe {
            instance.get_physical_device_properties2(physical_device, &mut props2);
        }
        // Read from props2 (which mutably borrows driver_props) before
        // reading driver_props directly, so props2's borrow ends first.
        let device_type = props2.properties.device_type;
        let driver_id = driver_props.driver_id;

        Ok(Arc::new(VkContext {
            entry,
            instance,
            debug_utils_instance,
            physical_device,
            device,
            external_semaphore_fd,
            external_memory_fd,
            image_drm_format_modifier_ext,
            image_drm_format_modifier,
            tfp_tiling_strategy: std::sync::OnceLock::new(),
            graphics_queue_family,
            graphics_queue,
            debug_messenger,
            driver_id,
            device_type,
        }))
    }

    /// True when the picked Vulkan device is a software rasterizer
    /// (`VK_PHYSICAL_DEVICE_TYPE_CPU`, i.e. llvmpipe/lavapipe).
    ///
    /// Fine for headless rendering/tests, but driving **real KMS
    /// scanout** off a software device hard-hangs the machine on
    /// hardware that can't scan out the CPU/host-memory buffer
    /// (observed: nouveau on Pascal — the GPU's atomic commit wedges).
    /// The scanout bring-up refuses by default when this is true;
    /// see `PlatformBackend::from_platform_init`. Venus (virtio-gpu
    /// passthrough) reports `VIRTUAL_GPU`, not `CPU`, so it is not
    /// caught here.
    #[must_use]
    pub fn is_software_rasterizer(&self) -> bool {
        self.device_type == vk::PhysicalDeviceType::CPU
    }
}

fn validation_layer_present(entry: &ash::Entry, name: &CStr) -> bool {
    match unsafe { entry.enumerate_instance_layer_properties() } {
        Ok(layers) => layers
            .iter()
            .any(|l| l.layer_name_as_c_str().map(|s| s == name).unwrap_or(false)),
        Err(_) => false,
    }
}

fn create_debug_messenger(
    debug_utils_instance: &ash::ext::debug_utils::Instance,
) -> Result<Option<vk::DebugUtilsMessengerEXT>, VkInitError> {
    // Match the validation-layer enable rule from `VkContext::new`:
    // debug builds always install the messenger; release builds only
    // when `YSERVER_VK_VALIDATION` is set. Without the messenger the
    // validation layer has nowhere to report VUIDs and the layer is
    // effectively silent.
    if !cfg!(debug_assertions) && std::env::var_os("YSERVER_VK_VALIDATION").is_none() {
        return Ok(None);
    }
    let info = vk::DebugUtilsMessengerCreateInfoEXT::default()
        .message_severity(
            vk::DebugUtilsMessageSeverityFlagsEXT::WARNING
                | vk::DebugUtilsMessageSeverityFlagsEXT::ERROR,
        )
        .message_type(
            vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
        )
        .pfn_user_callback(Some(vk_debug_callback));
    Ok(Some(unsafe {
        debug_utils_instance.create_debug_utils_messenger(&info, None)?
    }))
}

impl Drop for VkContext {
    fn drop(&mut self) {
        unsafe {
            // Wait for all queue work; tearing down with in-flight CBs
            // is undefined behaviour.
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            if let Some(m) = self.debug_messenger.take() {
                self.debug_utils_instance
                    .destroy_debug_utils_messenger(m, None);
            }
            self.instance.destroy_instance(None);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VkInitError {
    #[error("vulkan loader: {0}")]
    Loader(#[from] ash::LoadingError),
    #[error("vulkan: {0}")]
    Vk(vk::Result),
    #[error("no suitable physical device (need graphics queue + drm format modifier ext)")]
    NoSuitableDevice,
}

impl From<vk::Result> for VkInitError {
    fn from(r: vk::Result) -> Self {
        VkInitError::Vk(r)
    }
}

fn pick_physical_device(
    instance: &ash::Instance,
) -> Result<(vk::PhysicalDevice, u32), VkInitError> {
    let devices = unsafe { instance.enumerate_physical_devices() }?;

    let mut scored: Vec<(u32, vk::PhysicalDevice, u32)> = devices
        .into_iter()
        .filter_map(|pd| {
            let props = unsafe { instance.get_physical_device_properties(pd) };
            let queue_family = pick_graphics_queue_family(instance, pd)?;
            let score = match props.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 3,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
                vk::PhysicalDeviceType::VIRTUAL_GPU => 1,
                _ => 0,
            };
            Some((score, pd, queue_family))
        })
        .collect();
    scored.sort_by_key(|t| std::cmp::Reverse(t.0));
    scored
        .into_iter()
        .next()
        .map(|(_, pd, qf)| (pd, qf))
        .ok_or(VkInitError::NoSuitableDevice)
}

fn pick_graphics_queue_family(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Option<u32> {
    let qfp = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    qfp.iter().enumerate().find_map(|(i, p)| {
        if p.queue_flags
            .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::TRANSFER)
        {
            Some(u32::try_from(i).expect("queue family index fits in u32"))
        } else {
            None
        }
    })
}

unsafe extern "system" fn vk_debug_callback(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _ty: vk::DebugUtilsMessageTypeFlagsEXT,
    callback_data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    _user_data: *mut c_void,
) -> vk::Bool32 {
    // Validation can call this with a null callback_data on some
    // drivers; defend against that.
    if callback_data.is_null() {
        return vk::FALSE;
    }
    let data = unsafe { &*callback_data };
    let msg = if data.p_message.is_null() {
        "<no message>"
    } else {
        unsafe { CStr::from_ptr(data.p_message) }
            .to_str()
            .unwrap_or("<non-utf8 message>")
    };
    if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR) {
        log::error!("vk: {msg}");
    } else if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::WARNING) {
        log::warn!("vk: {msg}");
    }
    // INFO/VERBOSE intentionally suppressed — too noisy.
    vk::FALSE
}
