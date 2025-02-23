// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

use crate::{
    device::{Device, DeviceOwned},
    macros::{vulkan_bitflags, vulkan_enum},
    OomError, RequirementNotMet, RequiresOneOf, Version, VulkanError, VulkanObject,
};
use std::{
    error::Error,
    fmt::{Display, Error as FmtError, Formatter},
    fs::File,
    hash::{Hash, Hasher},
    mem::MaybeUninit,
    ptr,
    sync::Arc,
};

/// Used to provide synchronization between command buffers during their execution.
///
/// It is similar to a fence, except that it is purely on the GPU side. The CPU can't query a
/// semaphore's status or wait for it to be signaled.
#[derive(Debug)]
pub struct Semaphore {
    handle: ash::vk::Semaphore,
    device: Arc<Device>,

    export_handle_types: ExternalSemaphoreHandleTypes,

    must_put_in_pool: bool,
}

impl Semaphore {
    /// Creates a new `Semaphore`.
    #[inline]
    pub fn new(
        device: Arc<Device>,
        create_info: SemaphoreCreateInfo,
    ) -> Result<Semaphore, SemaphoreError> {
        Self::validate_new(&device, &create_info)?;

        unsafe { Ok(Self::new_unchecked(device, create_info)?) }
    }

    fn validate_new(
        device: &Device,
        create_info: &SemaphoreCreateInfo,
    ) -> Result<(), SemaphoreError> {
        let &SemaphoreCreateInfo {
            export_handle_types,
            _ne: _,
        } = create_info;

        if !export_handle_types.is_empty() {
            if !(device.api_version() >= Version::V1_1
                || device.enabled_extensions().khr_external_semaphore)
            {
                return Err(SemaphoreError::RequirementNotMet {
                    required_for: "`create_info.export_handle_types` is not empty",
                    requires_one_of: RequiresOneOf {
                        api_version: Some(Version::V1_1),
                        device_extensions: &["khr_external_semaphore"],
                        ..Default::default()
                    },
                });
            }

            // VUID-VkExportSemaphoreCreateInfo-handleTypes-parameter
            export_handle_types.validate_device(device)?;

            // VUID-VkExportSemaphoreCreateInfo-handleTypes-01124
            // TODO: `vkGetPhysicalDeviceExternalSemaphoreProperties` can only be called with one
            // handle type, so which one do we give it?
        }

        Ok(())
    }

    #[cfg_attr(not(feature = "document_unchecked"), doc(hidden))]
    #[inline]
    pub unsafe fn new_unchecked(
        device: Arc<Device>,
        create_info: SemaphoreCreateInfo,
    ) -> Result<Semaphore, VulkanError> {
        let SemaphoreCreateInfo {
            export_handle_types,
            _ne: _,
        } = create_info;

        let mut create_info_vk = ash::vk::SemaphoreCreateInfo {
            flags: ash::vk::SemaphoreCreateFlags::empty(),
            ..Default::default()
        };
        let mut export_semaphore_create_info_vk = None;

        if !export_handle_types.is_empty() {
            let _ = export_semaphore_create_info_vk.insert(ash::vk::ExportSemaphoreCreateInfo {
                handle_types: export_handle_types.into(),
                ..Default::default()
            });
        };

        if let Some(info) = export_semaphore_create_info_vk.as_mut() {
            info.p_next = create_info_vk.p_next;
            create_info_vk.p_next = info as *const _ as *const _;
        }

        let handle = {
            let fns = device.fns();
            let mut output = MaybeUninit::uninit();
            (fns.v1_0.create_semaphore)(
                device.internal_object(),
                &create_info_vk,
                ptr::null(),
                output.as_mut_ptr(),
            )
            .result()
            .map_err(VulkanError::from)?;
            output.assume_init()
        };

        Ok(Semaphore {
            device,
            handle,

            export_handle_types,

            must_put_in_pool: false,
        })
    }

    /// Takes a semaphore from the vulkano-provided semaphore pool.
    /// If the pool is empty, a new semaphore will be allocated.
    /// Upon `drop`, the semaphore is put back into the pool.
    ///
    /// For most applications, using the pool should be preferred,
    /// in order to avoid creating new semaphores every frame.
    #[inline]
    pub fn from_pool(device: Arc<Device>) -> Result<Semaphore, SemaphoreError> {
        let handle = device.semaphore_pool().lock().pop();
        let semaphore = match handle {
            Some(handle) => Semaphore {
                device,
                handle,

                export_handle_types: ExternalSemaphoreHandleTypes::empty(),

                must_put_in_pool: true,
            },
            None => {
                // Pool is empty, alloc new semaphore
                let mut semaphore = Semaphore::new(device, Default::default())?;
                semaphore.must_put_in_pool = true;
                semaphore
            }
        };

        Ok(semaphore)
    }

    /// Creates a new `Semaphore` from a raw object handle.
    ///
    /// # Safety
    ///
    /// - `handle` must be a valid Vulkan object handle created from `device`.
    /// - `create_info` must match the info used to create the object.
    #[inline]
    pub unsafe fn from_handle(
        device: Arc<Device>,
        handle: ash::vk::Semaphore,
        create_info: SemaphoreCreateInfo,
    ) -> Semaphore {
        let SemaphoreCreateInfo {
            export_handle_types,
            _ne: _,
        } = create_info;

        Semaphore {
            device,
            handle,

            export_handle_types,

            must_put_in_pool: false,
        }
    }

    /// Exports the semaphore into a POSIX file descriptor. The caller owns the returned `File`.
    ///
    /// # Safety
    ///
    /// - The semaphore must not be used, or have been used, to acquire a swapchain image.
    #[inline]
    pub unsafe fn export_fd(
        &self,
        handle_type: ExternalSemaphoreHandleType,
    ) -> Result<File, SemaphoreError> {
        self.validate_export_fd(handle_type)?;

        Ok(self.export_fd_unchecked(handle_type)?)
    }

    fn validate_export_fd(
        &self,
        handle_type: ExternalSemaphoreHandleType,
    ) -> Result<(), SemaphoreError> {
        if !self.device.enabled_extensions().khr_external_semaphore_fd {
            return Err(SemaphoreError::RequirementNotMet {
                required_for: "`export_fd`",
                requires_one_of: RequiresOneOf {
                    device_extensions: &["khr_external_semaphore_fd"],
                    ..Default::default()
                },
            });
        }

        // VUID-VkMemoryGetFdInfoKHR-handleType-parameter
        handle_type.validate_device(&self.device)?;

        // VUID-VkSemaphoreGetFdInfoKHR-handleType-01132
        if !self.export_handle_types.intersects(&handle_type.into()) {
            return Err(SemaphoreError::HandleTypeNotSupported { handle_type });
        }

        // VUID-VkSemaphoreGetFdInfoKHR-semaphore-01133
        // Can't validate for swapchain.

        // VUID-VkSemaphoreGetFdInfoKHR-handleType-01134
        // TODO:

        // VUID-VkSemaphoreGetFdInfoKHR-handleType-01135
        // TODO:

        // VUID-VkSemaphoreGetFdInfoKHR-handleType-01136
        if !matches!(
            handle_type,
            ExternalSemaphoreHandleType::OpaqueFd | ExternalSemaphoreHandleType::SyncFd
        ) {
            return Err(SemaphoreError::HandleTypeNotSupported { handle_type });
        }

        // VUID-VkSemaphoreGetFdInfoKHR-handleType-03253
        // TODO:

        // VUID-VkSemaphoreGetFdInfoKHR-handleType-03254
        // TODO:

        Ok(())
    }

    #[cfg(not(unix))]
    #[cfg_attr(not(feature = "document_unchecked"), doc(hidden))]
    #[inline]
    pub unsafe fn export_fd_unchecked(
        &self,
        _handle_type: ExternalSemaphoreHandleType,
    ) -> Result<File, VulkanError> {
        unreachable!("`khr_external_semaphore_fd` was somehow enabled on a non-Unix system");
    }

    #[cfg(unix)]
    #[cfg_attr(not(feature = "document_unchecked"), doc(hidden))]
    #[inline]
    pub unsafe fn export_fd_unchecked(
        &self,
        handle_type: ExternalSemaphoreHandleType,
    ) -> Result<File, VulkanError> {
        use std::os::unix::io::FromRawFd;

        let info = ash::vk::SemaphoreGetFdInfoKHR {
            semaphore: self.handle,
            handle_type: handle_type.into(),
            ..Default::default()
        };

        let mut output = MaybeUninit::uninit();
        let fns = self.device.fns();
        (fns.khr_external_semaphore_fd.get_semaphore_fd_khr)(
            self.device.internal_object(),
            &info,
            output.as_mut_ptr(),
        )
        .result()
        .map_err(VulkanError::from)?;

        Ok(File::from_raw_fd(output.assume_init()))
    }
}

impl Drop for Semaphore {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            if self.must_put_in_pool {
                let raw_sem = self.handle;
                self.device.semaphore_pool().lock().push(raw_sem);
            } else {
                let fns = self.device.fns();
                (fns.v1_0.destroy_semaphore)(
                    self.device.internal_object(),
                    self.handle,
                    ptr::null(),
                );
            }
        }
    }
}

unsafe impl VulkanObject for Semaphore {
    type Object = ash::vk::Semaphore;

    #[inline]
    fn internal_object(&self) -> ash::vk::Semaphore {
        self.handle
    }
}

unsafe impl DeviceOwned for Semaphore {
    #[inline]
    fn device(&self) -> &Arc<Device> {
        &self.device
    }
}

impl PartialEq for Semaphore {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.handle == other.handle && self.device() == other.device()
    }
}

impl Eq for Semaphore {}

impl Hash for Semaphore {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.handle.hash(state);
        self.device().hash(state);
    }
}

/// Parameters to create a new `Semaphore`.
#[derive(Clone, Debug)]
pub struct SemaphoreCreateInfo {
    /// The handle types that can be exported from the semaphore.
    ///
    /// The default value is [`ExternalSemaphoreHandleTypes::empty()`].
    pub export_handle_types: ExternalSemaphoreHandleTypes,

    pub _ne: crate::NonExhaustive,
}

impl Default for SemaphoreCreateInfo {
    #[inline]
    fn default() -> Self {
        Self {
            export_handle_types: ExternalSemaphoreHandleTypes::empty(),
            _ne: crate::NonExhaustive(()),
        }
    }
}

vulkan_enum! {
    /// The handle type used for Vulkan external semaphore APIs.
    #[non_exhaustive]
    ExternalSemaphoreHandleType = ExternalSemaphoreHandleTypeFlags(u32);

    // TODO: document
    OpaqueFd = OPAQUE_FD,

    // TODO: document
    OpaqueWin32 = OPAQUE_WIN32,

    // TODO: document
    OpaqueWin32Kmt = OPAQUE_WIN32_KMT,

    // TODO: document
    D3D12Fence = D3D12_FENCE,

    // TODO: document
    SyncFd = SYNC_FD,

    /*
    // TODO: document
    ZirconEvent = ZIRCON_EVENT_FUCHSIA {
        device_extensions: [fuchsia_external_semaphore],
    },
     */
}

vulkan_bitflags! {
    /// A mask of multiple external semaphore handle types.
    #[non_exhaustive]
    ExternalSemaphoreHandleTypes = ExternalSemaphoreHandleTypeFlags(u32);

    // TODO: document
    opaque_fd = OPAQUE_FD,

    // TODO: document
    opaque_win32 = OPAQUE_WIN32,

    // TODO: document
    opaque_win32_kmt = OPAQUE_WIN32_KMT,

    // TODO: document
    d3d12_fence = D3D12_FENCE,

    // TODO: document
    sync_fd = SYNC_FD,

    /*
    // TODO: document
    zircon_event = ZIRCON_EVENT_FUCHSIA {
        device_extensions: [fuchsia_external_semaphore],
    },
     */
}

impl From<ExternalSemaphoreHandleType> for ExternalSemaphoreHandleTypes {
    #[inline]
    fn from(val: ExternalSemaphoreHandleType) -> Self {
        let mut result = Self::empty();

        match val {
            ExternalSemaphoreHandleType::OpaqueFd => result.opaque_fd = true,
            ExternalSemaphoreHandleType::OpaqueWin32 => result.opaque_win32 = true,
            ExternalSemaphoreHandleType::OpaqueWin32Kmt => result.opaque_win32_kmt = true,
            ExternalSemaphoreHandleType::D3D12Fence => result.d3d12_fence = true,
            ExternalSemaphoreHandleType::SyncFd => result.sync_fd = true,
        }

        result
    }
}

vulkan_bitflags! {
    /// Additional parameters for a semaphore payload import.
    #[non_exhaustive]
    SemaphoreImportFlags = SemaphoreImportFlags(u32);

    /// The semaphore payload will be imported only temporarily, regardless of the permanence of the
    /// imported handle type.
    temporary = TEMPORARY,
}

/// The semaphore configuration to query in
/// [`PhysicalDevice::external_semaphore_properties`](crate::device::physical::PhysicalDevice::external_semaphore_properties).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExternalSemaphoreInfo {
    /// The external handle type that will be used with the semaphore.
    pub handle_type: ExternalSemaphoreHandleType,

    pub _ne: crate::NonExhaustive,
}

impl ExternalSemaphoreInfo {
    /// Returns an `ExternalSemaphoreInfo` with the specified `handle_type`.
    #[inline]
    pub fn handle_type(handle_type: ExternalSemaphoreHandleType) -> Self {
        Self {
            handle_type,
            _ne: crate::NonExhaustive(()),
        }
    }
}

/// The properties for exporting or importing external handles, when a semaphore is created
/// with a specific configuration.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ExternalSemaphoreProperties {
    /// Whether a handle can be exported to an external source with the queried
    /// external handle type.
    pub exportable: bool,

    /// Whether a handle can be imported from an external source with the queried
    /// external handle type.
    pub importable: bool,

    /// Which external handle types can be re-exported after the queried external handle type has
    /// been imported.
    pub export_from_imported_handle_types: ExternalSemaphoreHandleTypes,

    /// Which external handle types can be enabled along with the queried external handle type
    /// when creating the semaphore.
    pub compatible_handle_types: ExternalSemaphoreHandleTypes,
}

/// Error that can be returned from operations on a semaphore.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SemaphoreError {
    /// Not enough memory available.
    OomError(OomError),

    RequirementNotMet {
        required_for: &'static str,
        requires_one_of: RequiresOneOf,
    },

    /// The requested export handle type was not provided in `export_handle_types` when creating the
    /// semaphore.
    HandleTypeNotSupported {
        handle_type: ExternalSemaphoreHandleType,
    },
}

impl Error for SemaphoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OomError(err) => Some(err),
            _ => None,
        }
    }
}

impl Display for SemaphoreError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FmtError> {
        match self {
            Self::OomError(_) => write!(f, "not enough memory available"),
            Self::RequirementNotMet {
                required_for,
                requires_one_of,
            } => write!(
                f,
                "a requirement was not met for: {}; requires one of: {}",
                required_for, requires_one_of,
            ),
            Self::HandleTypeNotSupported { handle_type } => write!(
                f,
                "the requested export handle type ({:?}) was not provided in `export_handle_types` \
                when creating the semaphore",
                handle_type,
            ),
        }
    }
}

impl From<VulkanError> for SemaphoreError {
    fn from(err: VulkanError) -> Self {
        match err {
            e @ VulkanError::OutOfHostMemory | e @ VulkanError::OutOfDeviceMemory => {
                Self::OomError(e.into())
            }
            _ => panic!("unexpected error: {:?}", err),
        }
    }
}

impl From<OomError> for SemaphoreError {
    fn from(err: OomError) -> Self {
        Self::OomError(err)
    }
}

impl From<RequirementNotMet> for SemaphoreError {
    fn from(err: RequirementNotMet) -> Self {
        Self::RequirementNotMet {
            required_for: err.required_for,
            requires_one_of: err.requires_one_of,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ExternalSemaphoreHandleType;
    use crate::{
        device::{Device, DeviceCreateInfo, DeviceExtensions, QueueCreateInfo},
        instance::{Instance, InstanceCreateInfo, InstanceExtensions},
        sync::{ExternalSemaphoreHandleTypes, Semaphore, SemaphoreCreateInfo},
        VulkanLibrary, VulkanObject,
    };

    #[test]
    fn semaphore_create() {
        let (device, _) = gfx_dev_and_queue!();
        let _ = Semaphore::new(device, Default::default());
    }

    #[test]
    fn semaphore_pool() {
        let (device, _) = gfx_dev_and_queue!();

        assert_eq!(device.semaphore_pool().lock().len(), 0);
        let sem1_internal_obj = {
            let sem = Semaphore::from_pool(device.clone()).unwrap();
            assert_eq!(device.semaphore_pool().lock().len(), 0);
            sem.internal_object()
        };

        assert_eq!(device.semaphore_pool().lock().len(), 1);
        let sem2 = Semaphore::from_pool(device.clone()).unwrap();
        assert_eq!(device.semaphore_pool().lock().len(), 0);
        assert_eq!(sem2.internal_object(), sem1_internal_obj);
    }

    #[test]
    fn semaphore_export() {
        let library = match VulkanLibrary::new() {
            Ok(x) => x,
            Err(_) => return,
        };

        let instance = match Instance::new(
            library,
            InstanceCreateInfo {
                enabled_extensions: InstanceExtensions {
                    khr_get_physical_device_properties2: true,
                    khr_external_semaphore_capabilities: true,
                    ..InstanceExtensions::empty()
                },
                ..Default::default()
            },
        ) {
            Ok(x) => x,
            Err(_) => return,
        };

        let physical_device = match instance.enumerate_physical_devices() {
            Ok(mut x) => x.next().unwrap(),
            Err(_) => return,
        };

        let (device, _) = match Device::new(
            physical_device,
            DeviceCreateInfo {
                enabled_extensions: DeviceExtensions {
                    khr_external_semaphore: true,
                    khr_external_semaphore_fd: true,
                    ..DeviceExtensions::empty()
                },
                queue_create_infos: vec![QueueCreateInfo {
                    queue_family_index: 0,
                    ..Default::default()
                }],
                ..Default::default()
            },
        ) {
            Ok(x) => x,
            Err(_) => return,
        };

        let sem = Semaphore::new(
            device,
            SemaphoreCreateInfo {
                export_handle_types: ExternalSemaphoreHandleTypes {
                    opaque_fd: true,
                    ..ExternalSemaphoreHandleTypes::empty()
                },
                ..Default::default()
            },
        )
        .unwrap();
        let _fd = unsafe {
            sem.export_fd(ExternalSemaphoreHandleType::OpaqueFd)
                .unwrap()
        };
    }
}
