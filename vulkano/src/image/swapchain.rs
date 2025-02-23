// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

use super::{traits::ImageContent, ImageAccess, ImageDescriptorLayouts, ImageInner, ImageLayout};
use crate::{
    device::{Device, DeviceOwned},
    swapchain::{Swapchain, SwapchainAbstract},
    OomError,
};
use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

/// An image that is part of a swapchain.
///
/// Creating a `SwapchainImage` is automatically done when creating a swapchain.
///
/// A swapchain image is special in the sense that it can only be used after being acquired by
/// calling the `acquire` method on the swapchain. You have no way to know in advance which
/// swapchain image is going to be acquired, so you should keep all of them alive.
///
/// After a swapchain image has been acquired, you are free to perform all the usual operations
/// on it. When you are done you can then *present* the image (by calling the corresponding
/// method on the swapchain), which will have the effect of showing the content of the image to
/// the screen. Once an image has been presented, it can no longer be used unless it is acquired
/// again.
#[derive(Debug)]
pub struct SwapchainImage<W> {
    swapchain: Arc<Swapchain<W>>,
    image_index: u32,
}

impl<W> SwapchainImage<W>
where
    W: Send + Sync,
{
    /// Builds a `SwapchainImage` from raw components.
    ///
    /// This is an internal method that you shouldn't call.
    pub unsafe fn from_raw(
        swapchain: Arc<Swapchain<W>>,
        image_index: u32,
    ) -> Result<Arc<SwapchainImage<W>>, OomError> {
        Ok(Arc::new(SwapchainImage {
            swapchain,
            image_index,
        }))
    }

    /// Returns the swapchain this image belongs to.
    pub fn swapchain(&self) -> &Arc<Swapchain<W>> {
        &self.swapchain
    }

    fn my_image(&self) -> ImageInner<'_> {
        self.swapchain.raw_image(self.image_index).unwrap()
    }

    fn layout_initialized(&self) {
        self.swapchain.image_layout_initialized(self.image_index);
    }

    fn is_layout_initialized(&self) -> bool {
        self.swapchain.is_image_layout_initialized(self.image_index)
    }
}

unsafe impl<W> DeviceOwned for SwapchainImage<W> {
    fn device(&self) -> &Arc<Device> {
        self.swapchain.device()
    }
}

unsafe impl<W> ImageAccess for SwapchainImage<W>
where
    W: Send + Sync,
{
    fn inner(&self) -> ImageInner<'_> {
        self.my_image()
    }

    fn initial_layout_requirement(&self) -> ImageLayout {
        ImageLayout::PresentSrc
    }

    fn final_layout_requirement(&self) -> ImageLayout {
        ImageLayout::PresentSrc
    }

    fn descriptor_layouts(&self) -> Option<ImageDescriptorLayouts> {
        Some(ImageDescriptorLayouts {
            storage_image: ImageLayout::General,
            combined_image_sampler: ImageLayout::ShaderReadOnlyOptimal,
            sampled_image: ImageLayout::ShaderReadOnlyOptimal,
            input_attachment: ImageLayout::ShaderReadOnlyOptimal,
        })
    }

    unsafe fn layout_initialized(&self) {
        self.layout_initialized();
    }

    fn is_layout_initialized(&self) -> bool {
        self.is_layout_initialized()
    }
}

unsafe impl<P, W> ImageContent<P> for SwapchainImage<W>
where
    W: Send + Sync,
{
    fn matches_format(&self) -> bool {
        true // FIXME:
    }
}

impl<W> PartialEq for SwapchainImage<W>
where
    W: Send + Sync,
{
    fn eq(&self, other: &Self) -> bool {
        self.inner() == other.inner()
    }
}

impl<W> Eq for SwapchainImage<W> where W: Send + Sync {}

impl<W> Hash for SwapchainImage<W>
where
    W: Send + Sync,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner().hash(state);
    }
}
