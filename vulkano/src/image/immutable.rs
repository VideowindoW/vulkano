// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

use super::{
    sys::UnsafeImage, traits::ImageContent, ImageAccess, ImageCreateFlags, ImageCreationError,
    ImageDescriptorLayouts, ImageDimensions, ImageInner, ImageLayout, ImageSubresourceLayers,
    ImageUsage, MipmapsCount,
};
use crate::{
    buffer::{BufferAccess, BufferContents, BufferUsage, CpuAccessibleBuffer},
    command_buffer::{
        AutoCommandBufferBuilder, BlitImageInfo, CommandBufferBeginError, CommandBufferExecFuture,
        CommandBufferUsage, CopyBufferToImageInfo, ImageBlit, PrimaryCommandBuffer,
    },
    device::{Device, DeviceOwned, Queue},
    format::Format,
    image::sys::UnsafeImageCreateInfo,
    memory::{
        pool::{
            AllocFromRequirementsFilter, AllocLayout, MappingRequirement, MemoryPoolAlloc,
            PotentialDedicatedAllocation, StandardMemoryPoolAlloc,
        },
        DedicatedAllocation, DeviceMemoryError, MemoryPool,
    },
    sampler::Filter,
    sync::{NowFuture, Sharing},
    OomError,
};
use smallvec::SmallVec;
use std::{
    error::Error,
    fmt::{Display, Error as FmtError, Formatter},
    hash::{Hash, Hasher},
    sync::Arc,
};

/// Image whose purpose is to be used for read-only purposes. You can write to the image once,
/// but then you must only ever read from it.
// TODO: type (2D, 3D, array, etc.) as template parameter
#[derive(Debug)]
pub struct ImmutableImage<A = PotentialDedicatedAllocation<StandardMemoryPoolAlloc>> {
    image: Arc<UnsafeImage>,
    dimensions: ImageDimensions,
    _memory: A,
    layout: ImageLayout,
}

fn has_mipmaps(mipmaps: MipmapsCount) -> bool {
    match mipmaps {
        MipmapsCount::One => false,
        MipmapsCount::Log2 => true,
        MipmapsCount::Specific(x) => x > 1,
    }
}

fn generate_mipmaps<L>(
    cbb: &mut AutoCommandBufferBuilder<L>,
    image: Arc<dyn ImageAccess>,
    dimensions: ImageDimensions,
    _layout: ImageLayout,
) {
    for level in 1..image.mip_levels() {
        let src_size = dimensions
            .mip_level_dimensions(level - 1)
            .unwrap()
            .width_height_depth();
        let dst_size = dimensions
            .mip_level_dimensions(level)
            .unwrap()
            .width_height_depth();

        cbb.blit_image(BlitImageInfo {
            regions: [ImageBlit {
                src_subresource: ImageSubresourceLayers {
                    mip_level: level - 1,
                    ..image.subresource_layers()
                },
                src_offsets: [[0; 3], src_size],
                dst_subresource: ImageSubresourceLayers {
                    mip_level: level,
                    ..image.subresource_layers()
                },
                dst_offsets: [[0; 3], dst_size],
                ..Default::default()
            }]
            .into(),
            filter: Filter::Linear,
            ..BlitImageInfo::images(image.clone(), image.clone())
        })
        .expect("failed to blit a mip map to image!");
    }
}

impl ImmutableImage {
    #[deprecated(note = "use ImmutableImage::uninitialized instead")]
    pub fn new(
        device: Arc<Device>,
        dimensions: ImageDimensions,
        format: Format,
        queue_family_indices: impl IntoIterator<Item = u32>,
    ) -> Result<Arc<ImmutableImage>, ImmutableImageCreationError> {
        #[allow(deprecated)]
        ImmutableImage::with_mipmaps(
            device,
            dimensions,
            format,
            MipmapsCount::One,
            queue_family_indices,
        )
    }

    #[deprecated(note = "use ImmutableImage::uninitialized instead")]
    pub fn with_mipmaps(
        device: Arc<Device>,
        dimensions: ImageDimensions,
        format: Format,
        mip_levels: impl Into<MipmapsCount>,
        queue_family_indices: impl IntoIterator<Item = u32>,
    ) -> Result<Arc<ImmutableImage>, ImmutableImageCreationError> {
        let usage = ImageUsage {
            transfer_src: true, // for blits
            transfer_dst: true,
            sampled: true,
            ..ImageUsage::empty()
        };

        let flags = ImageCreateFlags::empty();

        let (image, _) = ImmutableImage::uninitialized(
            device,
            dimensions,
            format,
            mip_levels,
            usage,
            flags,
            ImageLayout::ShaderReadOnlyOptimal,
            queue_family_indices,
        )?;
        Ok(image)
    }

    /// Builds an uninitialized immutable image.
    ///
    /// Returns two things: the image, and a special access that should be used for the initial
    /// upload to the image.
    pub fn uninitialized(
        device: Arc<Device>,
        dimensions: ImageDimensions,
        format: Format,
        mip_levels: impl Into<MipmapsCount>,
        usage: ImageUsage,
        flags: ImageCreateFlags,
        layout: ImageLayout,
        queue_family_indices: impl IntoIterator<Item = u32>,
    ) -> Result<(Arc<ImmutableImage>, Arc<ImmutableImageInitialization>), ImmutableImageCreationError>
    {
        let queue_family_indices: SmallVec<[_; 4]> = queue_family_indices.into_iter().collect();

        let image = UnsafeImage::new(
            device.clone(),
            UnsafeImageCreateInfo {
                dimensions,
                format: Some(format),
                mip_levels: match mip_levels.into() {
                    MipmapsCount::Specific(num) => num,
                    MipmapsCount::Log2 => dimensions.max_mip_levels(),
                    MipmapsCount::One => 1,
                },
                usage,
                sharing: if queue_family_indices.len() >= 2 {
                    Sharing::Concurrent(queue_family_indices)
                } else {
                    Sharing::Exclusive
                },
                mutable_format: flags.mutable_format,
                cube_compatible: flags.cube_compatible,
                array_2d_compatible: flags.array_2d_compatible,
                block_texel_view_compatible: flags.block_texel_view_compatible,
                ..Default::default()
            },
        )?;

        let mem_reqs = image.memory_requirements();
        let memory = MemoryPool::alloc_from_requirements(
            &device.standard_memory_pool(),
            &mem_reqs,
            AllocLayout::Optimal,
            MappingRequirement::DoNotMap,
            Some(DedicatedAllocation::Image(&image)),
            |t| {
                if t.property_flags.device_local {
                    AllocFromRequirementsFilter::Preferred
                } else {
                    AllocFromRequirementsFilter::Allowed
                }
            },
        )?;
        debug_assert!((memory.offset() % mem_reqs.alignment) == 0);
        unsafe {
            image.bind_memory(memory.memory(), memory.offset())?;
        }

        let image = Arc::new(ImmutableImage {
            image,
            _memory: memory,
            dimensions,
            layout,
        });

        let init = Arc::new(ImmutableImageInitialization {
            image: image.clone(),
        });

        Ok((image, init))
    }

    /// Construct an ImmutableImage from the contents of `iter`.
    pub fn from_iter<Px, I>(
        iter: I,
        dimensions: ImageDimensions,
        mip_levels: MipmapsCount,
        format: Format,
        queue: Arc<Queue>,
    ) -> Result<(Arc<Self>, CommandBufferExecFuture<NowFuture>), ImmutableImageCreationError>
    where
        [Px]: BufferContents,
        I: IntoIterator<Item = Px>,
        I::IntoIter: ExactSizeIterator,
    {
        let source = CpuAccessibleBuffer::from_iter(
            queue.device().clone(),
            BufferUsage {
                transfer_src: true,
                ..BufferUsage::empty()
            },
            false,
            iter,
        )?;
        ImmutableImage::from_buffer(source, dimensions, mip_levels, format, queue)
    }

    /// Construct an ImmutableImage containing a copy of the data in `source`.
    pub fn from_buffer(
        source: Arc<dyn BufferAccess>,
        dimensions: ImageDimensions,
        mip_levels: MipmapsCount,
        format: Format,
        queue: Arc<Queue>,
    ) -> Result<(Arc<Self>, CommandBufferExecFuture<NowFuture>), ImmutableImageCreationError> {
        let need_to_generate_mipmaps = has_mipmaps(mip_levels);
        let usage = ImageUsage {
            transfer_dst: true,
            transfer_src: need_to_generate_mipmaps,
            sampled: true,
            ..ImageUsage::empty()
        };
        let flags = ImageCreateFlags::empty();
        let layout = ImageLayout::ShaderReadOnlyOptimal;

        let (image, initializer) = ImmutableImage::uninitialized(
            source.device().clone(),
            dimensions,
            format,
            mip_levels,
            usage,
            flags,
            layout,
            source
                .device()
                .active_queue_family_indices()
                .iter()
                .copied(),
        )?;

        let mut cbb = AutoCommandBufferBuilder::primary(
            source.device().clone(),
            queue.queue_family_index(),
            CommandBufferUsage::MultipleSubmit,
        )?;
        cbb.copy_buffer_to_image(CopyBufferToImageInfo::buffer_image(source, initializer))
            .unwrap();

        if need_to_generate_mipmaps {
            generate_mipmaps(
                &mut cbb,
                image.clone(),
                image.dimensions,
                ImageLayout::ShaderReadOnlyOptimal,
            );
        }

        let cb = cbb.build().unwrap();

        let future = match cb.execute(queue) {
            Ok(f) => f,
            Err(e) => unreachable!("{:?}", e),
        };

        Ok((image, future))
    }
}

unsafe impl<A> DeviceOwned for ImmutableImage<A> {
    fn device(&self) -> &Arc<Device> {
        self.image.device()
    }
}

unsafe impl<A> ImageAccess for ImmutableImage<A>
where
    A: MemoryPoolAlloc,
{
    fn inner(&self) -> ImageInner<'_> {
        ImageInner {
            image: &self.image,
            first_layer: 0,
            num_layers: self.image.dimensions().array_layers(),
            first_mipmap_level: 0,
            num_mipmap_levels: self.image.mip_levels(),
        }
    }

    fn is_layout_initialized(&self) -> bool {
        true
    }

    fn initial_layout_requirement(&self) -> ImageLayout {
        self.layout
    }

    fn final_layout_requirement(&self) -> ImageLayout {
        self.layout
    }

    fn descriptor_layouts(&self) -> Option<ImageDescriptorLayouts> {
        Some(ImageDescriptorLayouts {
            storage_image: ImageLayout::General,
            combined_image_sampler: self.layout,
            sampled_image: self.layout,
            input_attachment: self.layout,
        })
    }
}

unsafe impl<P, A> ImageContent<P> for ImmutableImage<A>
where
    A: MemoryPoolAlloc,
{
    fn matches_format(&self) -> bool {
        true // FIXME:
    }
}

impl<A> PartialEq for ImmutableImage<A>
where
    A: MemoryPoolAlloc,
{
    fn eq(&self, other: &Self) -> bool {
        self.inner() == other.inner()
    }
}

impl<A> Eq for ImmutableImage<A> where A: MemoryPoolAlloc {}

impl<A> Hash for ImmutableImage<A>
where
    A: MemoryPoolAlloc,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner().hash(state);
    }
}

// Must not implement Clone, as that would lead to multiple `used` values.
pub struct ImmutableImageInitialization<A = PotentialDedicatedAllocation<StandardMemoryPoolAlloc>> {
    image: Arc<ImmutableImage<A>>,
}

unsafe impl<A> DeviceOwned for ImmutableImageInitialization<A> {
    fn device(&self) -> &Arc<Device> {
        self.image.device()
    }
}

unsafe impl<A> ImageAccess for ImmutableImageInitialization<A>
where
    A: MemoryPoolAlloc,
{
    fn inner(&self) -> ImageInner<'_> {
        self.image.inner()
    }

    fn initial_layout_requirement(&self) -> ImageLayout {
        ImageLayout::Undefined
    }

    fn final_layout_requirement(&self) -> ImageLayout {
        self.image.layout
    }

    fn descriptor_layouts(&self) -> Option<ImageDescriptorLayouts> {
        None
    }
}

impl<A> PartialEq for ImmutableImageInitialization<A>
where
    A: MemoryPoolAlloc,
{
    fn eq(&self, other: &Self) -> bool {
        self.inner() == other.inner()
    }
}

impl<A> Eq for ImmutableImageInitialization<A> where A: MemoryPoolAlloc {}

impl<A> Hash for ImmutableImageInitialization<A>
where
    A: MemoryPoolAlloc,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner().hash(state);
    }
}

#[derive(Clone, Debug)]
pub enum ImmutableImageCreationError {
    ImageCreationError(ImageCreationError),
    DeviceMemoryAllocationError(DeviceMemoryError),
    CommandBufferBeginError(CommandBufferBeginError),
}

impl Error for ImmutableImageCreationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ImageCreationError(err) => Some(err),
            Self::DeviceMemoryAllocationError(err) => Some(err),
            Self::CommandBufferBeginError(err) => Some(err),
        }
    }
}

impl Display for ImmutableImageCreationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FmtError> {
        match self {
            Self::ImageCreationError(err) => err.fmt(f),
            Self::DeviceMemoryAllocationError(err) => err.fmt(f),
            Self::CommandBufferBeginError(err) => err.fmt(f),
        }
    }
}

impl From<ImageCreationError> for ImmutableImageCreationError {
    fn from(err: ImageCreationError) -> Self {
        Self::ImageCreationError(err)
    }
}

impl From<DeviceMemoryError> for ImmutableImageCreationError {
    fn from(err: DeviceMemoryError) -> Self {
        Self::DeviceMemoryAllocationError(err)
    }
}

impl From<OomError> for ImmutableImageCreationError {
    fn from(err: OomError) -> Self {
        Self::DeviceMemoryAllocationError(err.into())
    }
}

impl From<CommandBufferBeginError> for ImmutableImageCreationError {
    fn from(err: CommandBufferBeginError) -> Self {
        Self::CommandBufferBeginError(err)
    }
}
